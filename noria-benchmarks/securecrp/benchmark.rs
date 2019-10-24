extern crate csv;
use csv::Writer;
use clap::value_t_or_exit;
use hdrhistogram::Histogram;
use noria::{Builder, FrontierStrategy, ReuseConfigType};
use rand::seq::SliceRandom;
use slog::{crit, debug, error, info, o, trace, warn, Logger};
use std::collections::{HashMap, HashSet};
use std::time::{Instant, Duration};
use std::thread;
use noria::{DurabilityMode, PersistenceParameters};

const PAPERS_PER_REVIEWER: usize = 3;

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, Ord, PartialOrd)]
enum Operation {
    ReadPaperList,
}

impl std::fmt::Display for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match *self {
            Operation::ReadPaperList => write!(f, "plist"),
        }
    }
}

struct Paper {
    accepted: bool,
    title: String,
    authors: Vec<usize>,
}

#[derive(Debug)]
struct Review {
    paper: usize,
    rating: usize,
    confidence: usize,
}

fn main() {
    use clap::{App, Arg};
    let args = App::new("manualgraph")
        .version("0.1")
        .about("Benchmarks HotCRP-like application with security policies.")
        .arg(
            Arg::with_name("reuse")
                .long("reuse")
                .default_value("no")
                .possible_values(&["no", "finkelstein", "relaxed", "full"])
                .help("Query reuse algorithm"),
        )
        .arg(
            Arg::with_name("materialization")
                .long("materialization")
                .short("m")
                .default_value("full")
                .possible_values(&["full", "partial", "shallow-readers", "shallow-all"])
                .help("Set materialization strategy for the benchmark"),
        )
        .arg(
            Arg::with_name("source")
                .long("source")
                .default_value("https://openreview.net/group?id=ICLR.cc/2018/Conference")
                .takes_value(true)
                .help("Source to pull paper data from"),
        )
        .arg(
            Arg::with_name("schema")
                .long("schema")
                .short("s")
                .required(true)
                .default_value("jeeves_schema.sql")
                .help("SQL schema file"),
        )
        .arg(
            Arg::with_name("queries")
                .long("queries")
                .short("q")
                .required(true)
                .default_value("jeeves_queries.sql")
                .help("SQL query file"),
        )
        .arg(
            Arg::with_name("policies")
                .long("policies")
                .short("p")
                .required(true)
                .default_value("jeeves_policies.json")
                .help("Security policies file"),
        )
        .arg(
            Arg::with_name("npapers")
                .short("n")
                .takes_value(true)
                .default_value("10000")
                .help("Only fetch first n papers"),
        )
        .arg(
            Arg::with_name("logged-in")
                .short("l")
                .default_value("1.0")
                .help("Fraction of users that are logged in."),
        )
        .arg(
            Arg::with_name("iter")
                .long("iter")
                .default_value("1")
                .help("Number of iterations to run"),
        )
        .arg(
            Arg::with_name("graph")
                .short("g")
                .takes_value(true)
                .help("File to dump application's soup graph, if set"),
        )
        .arg(
            Arg::with_name("verbose")
                .short("v")
                .multiple(true)
                .help("Enable verbose output"),
        )
        .get_matches();
    let verbose = args.occurrences_of("verbose");
    let loggedf = value_t_or_exit!(args, "logged-in", f64);
    let source = value_t_or_exit!(args, "source", url::Url);

    assert!(loggedf >= 0.0);
    assert!(loggedf <= 1.0);

    let log = if verbose != 0 {
        noria::logger_pls()
    } else {
        Logger::root(slog::Discard, o!())
    };
    let mut rng = rand::thread_rng();

    let conf = source
        .query_pairs()
        .find(|(arg, _)| arg == "id")
        .expect("could not find conference id in url")
        .1;
    info!(log, "fetching source data"; "conf" => &*conf);
    // https://openreview.net/api/#/Notes/findNotes
    let url = format!(
        "https://openreview.net/notes?invitation={}%2F-%2FBlind_Submission&limit={}",
        url::percent_encoding::utf8_percent_encode(
            &*conf,
            url::percent_encoding::DEFAULT_ENCODE_SET
        ),
        value_t_or_exit!(args, "npapers", usize),
    );
    debug!(log, "sending request for paper list"; "url" => &url);
    let all = reqwest::get(&url)
        .expect("failed to fetch source")
        .json::<serde_json::Value>()
        .expect("invalid source json");
    let all = all
        .as_object()
        .expect("root of source data is not a json object as expected");
    let all = all
        .get("notes")
        .and_then(|a| a.as_array())
        .expect("source data has a weird structure");
    let url = format!(
        "https://openreview.net/notes?invitation={}%2F-%2FAcceptance_Decision&limit=10000",
        url::percent_encoding::utf8_percent_encode(
            &*conf,
            url::percent_encoding::DEFAULT_ENCODE_SET
        )
    );
    debug!(log, "fetching list of accepted papers"; "url" => &url);
    let accept = reqwest::get(&url)
        .expect("failed to fetch accepted list")
        .json::<serde_json::Value>()
        .expect("invalid acceptance list json");
    let accept = accept
        .as_object()
        .expect("root of acceptance list is not a json object as expected");
    let accept = accept
        .get("notes")
        .and_then(|a| a.as_array())
        .expect("acceptance list data has a weird structure");
    let mut accepted = HashSet::new();
    for decision in accept {
        let decision = decision.as_object().expect("acceptance info is weird");
        let id = decision["forum"]
            .as_str()
            .expect("listed acceptance forum is not a string");
        let was_accepted = decision["content"]
            .as_object()
            .expect("acceptance info content is weird")["decision"]
            .as_str()
            .expect("listed acceptance decision is not a string")
            .starts_with("Accept");
        if was_accepted {
            trace!(log, "noted acceptance decision"; "paper" => id);
            accepted.insert(id);
        }
    }
    let mut author_set = HashMap::new();
    let mut authors = Vec::new();
    let mut papers = Vec::new();
    let mut reviews = Vec::new();
    debug!(log, "processing paper list"; "n" => all.len());
    for paper in all {
        let paper = paper.as_object().expect("paper info isn't a json object");
        let id = paper["id"].as_str().expect("paper id is weird");
        let number = paper["number"].as_u64().expect("paper number is weird");
        let content = paper["content"]
            .as_object()
            .expect("paper info doesn't have content");
        let title = content["title"].as_str().unwrap().to_string();
        let authors: Vec<_> = content["authorids"]
            .as_array()
            .expect("author list is not an array")
            .iter()
            .map(|author| {
                let author = author.as_str().expect("author id is not a string");
                *author_set.entry(author.to_string()).or_insert_with(|| {
                    trace!(log, "adding author"; "name" => author, "uid" => author.len() + 1);
                    authors.push(author);
                    authors.len() - 1
                })
            })
            .collect();


        let pid = papers.len() + 1;
        trace!(log, "adding paper"; "title" => &title, "id" => pid, "accepted" => accepted.contains(id));
        papers.push(Paper {
            title,
            accepted: accepted.contains(id),
            authors,
        });

//    thread::sleep(time::Duration::from_millis(2000));
//    let _ = backend.login(make_user(user)).is_ok();

        let url = format!(
            "https://openreview.net/notes?forum={}&invitation={}/-/Paper{}/Official_Review",
            url::percent_encoding::utf8_percent_encode(
                &*id,
                url::percent_encoding::DEFAULT_ENCODE_SET
            ),
            url::percent_encoding::utf8_percent_encode(
                &*conf,
                url::percent_encoding::DEFAULT_ENCODE_SET
            ),
            format!("{}", number),
        );
        trace!(log, "fetching paper reviews"; "url" => &url);
        let paper_reviews = reqwest::get(&url)
            .expect("failed to fetch paper reviews")
            .json::<serde_json::Value>()
            .expect("invalid paper review json");
        let paper_reviews = paper_reviews
            .as_object()
            .expect("paper reviews is not a json object as expected");
        let paper_reviews = paper_reviews
            .get("notes")
            .and_then(|rs| rs.as_array())
            .expect("paper reviews has a weird structure");
        for review in paper_reviews {
            let content = review.as_object().expect("review was not an object")["content"]
                .as_object()
                .expect("review did not have regular contents");
            let r = Review {
                paper: pid,
                rating: content["rating"]
                    .as_str()
                    .expect("rating wasn't a string")
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .trim_end_matches(':')
                    .parse()
                    .expect("rating did not start with a number"),
                confidence: content["confidence"]
                    .as_str()
                    .expect("confidence wasn't a string")
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .trim_end_matches(':')
                    .parse()
                    .expect("confidence did not start with a number"),
            };
            trace!(log, "adding review"; "rating" => r.rating, "confidence" => r.confidence);
            reviews.push(r);
        }
    }

    drop(author_set);
    let nusers = authors.len();
    let mut nlogged = (loggedf * nusers as f64) as usize;

    // let's compute the number of reviewers
    // we know the number of reviews
    // we have fixed the number of reviews per reviewer
    // and we assume every reviewer is an author
    let nreviewers = (reviews.len() + (PAPERS_PER_REVIEWER - 1)) / PAPERS_PER_REVIEWER;

    println!("# nauthors: {}", authors.len());
    println!("# nreviewers: {}", nreviewers);
    println!("# npapers: {}", papers.len());
    println!("# nreviews: {}", reviews.len());
    println!(
        "# materialization: {}",
        args.value_of("materialization").unwrap()
    );

    let mut cold_stats = HashMap::new();
    let mut warm_stats = HashMap::new();
    let iter = value_t_or_exit!(args, "iter", usize);
    //    let loggedfs = vec![0.0, 0.003, 0.1, 0.5, 1.0];
    let loggedfs = vec![0.2];
    let mut wtr = Writer::from_path("tmp.csv").unwrap();
    for &lfrac in loggedfs.iter() {
        //    for iter in 1..=iter {
        let mut lf = lfrac;
        if lf > 0.0 && lf < 0.01 {
            lf = 1.0/(nreviewers as f32);
        }
        info!(log, "starting up noria"; "loggedf" => lf);
        let mut nlogged = (lf * nreviewers as f32) as usize;
        if lf != 0.0 && lf < 0.01 {
            nlogged = 1;
        }
        println!("# logged-in users: {}", nlogged);

        info!(log, "starting up noria"; "iteration" => iter);
        debug!(log, "configuring noria");
        let mut g = Builder::default();
        match args.value_of("reuse").unwrap() {
            "finkelstein" => g.set_reuse(ReuseConfigType::Finkelstein),
            "full" => g.set_reuse(ReuseConfigType::Full),
            "no" => g.set_reuse(ReuseConfigType::NoReuse),
            "relaxed" => g.set_reuse(ReuseConfigType::Relaxed),
            _ => unreachable!(),
        }

        match args.value_of("materialization").unwrap() {
            "full" => {
                g.disable_partial();
            }
            "partial" => {}
            "shallow-readers" => {
                g.set_frontier_strategy(FrontierStrategy::Readers);
            }
            "shallow-all" => {
                g.set_frontier_strategy(FrontierStrategy::AllPartial);
            }
            _ => unreachable!(),
        }
        g.set_sharding(None);
        if verbose > 1 {
            println!("NORIA IS verbose");
            g.log_with(log.clone());
        }
        g.log_with(log.clone());       
        g.set_persistence(PersistenceParameters::new(
            DurabilityMode::MemoryOnly,
            Duration::from_millis(1),
            Some(String::from("secure_crp")),
            1,
        ));
        
        debug!(log, "spinning up");
        let mut g = g.start_simple().unwrap();
        debug!(log, "noria ready");

        let init = Instant::now();
        thread::sleep(Duration::from_millis(2000));
        // Recipe Installation
        info!(log, "setting up database schema");
        debug!(log, "setting up initial schema");
        g.install_recipe(
            std::fs::read_to_string(args.value_of("schema").unwrap())
                .expect("failed to read schema file"),
        )
            .expect("failed to load initial schema");
        debug!(log, "adding security policies");
        g.on_worker(|w| {
            w.set_security_config(
                std::fs::read_to_string(args.value_of("policies").unwrap())
                    .expect("failed to read policy file"),
            )
        }).unwrap();
        debug!(log, "adding queries");
        g.extend_recipe(
            std::fs::read_to_string(args.value_of("queries").unwrap())
                .expect("failed to read queries file"),
        )
            .expect("failed to load initial schema");
        debug!(log, "database schema setup done");
        
        let mut memstats = |g: &mut noria::SyncHandle<_>, at| {
            if let Ok(mem) = std::fs::read_to_string("/proc/self/statm") {
                debug!(log, "extracing process memory stats"; "at" => at);
                let vmrss = mem.split_whitespace().nth(2 - 1).unwrap();
                let data = mem.split_whitespace().nth(6 - 1).unwrap();
                println!("# VmRSS @ {}: {} ", at, vmrss);
                println!("# VmData @ {}: {} ", at, data);
            }

            debug!(log, "extracing materialization memory stats"; "at" => at);
            let mut reader_mem = 0;
            let mut base_mem = 0;
            let mut mem = 0;
            let stats = g.statistics().unwrap();
            for (_, nstats) in stats.values() {
                for nstat in nstats.values() {
                    if nstat.desc == "B" {
                        base_mem += nstat.mem_size;
                    } else if nstat.desc == "reader node" {
                        reader_mem += nstat.mem_size;
                    } else {
                        mem += nstat.mem_size;
                    }
                }
            }

            wtr.write_record(&[format!("{}", lf),
                               format!("{}", nlogged),
                               format!("{}", at),
                               format!("{}", base_mem),
                               format!("{}", reader_mem),
                               format!("{}", mem)]);
            
            println!("# base memory @ {}: {}", at, base_mem);
            println!("# reader memory @ {}: {}", at, reader_mem);
            println!("# materialization memory @ {}: {}", at, mem);
        };

        info!(log, "starting db population");
        debug!(log, "getting handles to tables");
        let mut user_profile = g.table("UserProfile").unwrap().into_sync();
        let mut paper = g.table("Paper").unwrap().into_sync();
        let mut coauthor = g.table("PaperCoauthor").unwrap().into_sync();
        let mut version = g.table("PaperVersion").unwrap().into_sync();
        let mut review_assignment = g.table("ReviewAssignment").unwrap().into_sync();
        let mut review = g.table("Review").unwrap().into_sync();
        debug!(log, "creating users"; "n" => nusers);
        user_profile
            .perform_all(authors.iter().enumerate().map(|(i, &email)| {
                vec![
                    format!("{}", i + 1).into(),
                    email.into(),
                    email.into(),
                    "university".into(),
                    "0".into(),
                    if i == 0 {
                        "chair".into()
                    } else if i < nreviewers {
                        "pc".into()
                    } else {
                        "normal".into()
                    },
                ]
            }))
            .unwrap();
        debug!(log, "registering papers");
        let start = Instant::now();
        paper
            .perform_all(papers.iter().enumerate().map(|(i, p)| {
                vec![
                    (i + 1).into(),
                    format!("{}", p.authors[0] + 1).into(),
                    if p.accepted { 1 } else { 0 }.into(),
                ]
            }))
            .unwrap();
        println!(
            "# paper registration: {} in {:?}",
            papers.len(),
            start.elapsed()
        );
        trace!(log, "also registering paper version");
        version
            .perform_all(papers.iter().enumerate().map(|(i, p)| {
                vec![
                    (i + 1).into(),
                    (&*p.title).into(),
                    "Text".into(),
                    "Abstract".into(),
                    "0".into(),
                ]
            }))
            .unwrap();
        println!(
            "# paper + version: {} in {:?}",
            papers.len(),
            start.elapsed()
        );
        debug!(log, "registering paper authors");
        let start = Instant::now();
        let mut npauthors = 0;
        coauthor
            .perform_all(papers.iter().enumerate().flat_map(|(i, p)| {
                // XXX: should first author be repeated here?
                npauthors += p.authors.len();
                p.authors
                    .iter()
                    .map(move |&a| vec![(i + 1).into(), format!("{}", a + 1).into()])
            }))
            .unwrap();
        println!("# paper authors: {} in {:?}", npauthors, start.elapsed());
        debug!(log, "registering reviews");
        reviews.shuffle(&mut rng);
        // assume all reviews have been submitted
        trace!(log, "register assignments");
        let start = Instant::now();
        let reva_rows: Vec<Vec<std::string::String>> = reviews
                    .chunks(PAPERS_PER_REVIEWER)
                    .enumerate()
                    .flat_map(|(i, rs)| {
                        rs.iter().map(move |r| {
                            vec![format!("{}",r.paper).into(), format!("{}", i + 1).into(),
                                 format!("{},{}", r.rating, r.confidence).into()]
                        })
                    }).collect();
//        println!("reviews: {:#?}", reviews);
//        println!("reva_rows: {:?}", reva_rows);
        
        review_assignment
            .perform_all(
                reviews
                    .chunks(PAPERS_PER_REVIEWER)
                    .enumerate()
                    .flat_map(|(i, rs)| {
                        // TODO: don't review own paper
                        rs.iter().map(move |r| {
                            vec![r.paper.into(), format!("{}", i + 1).into(),
                                 format!("{},{}", r.rating, r.confidence).into()]
                        })
                    }),
            )
            .unwrap();
        println!(
            "# review assignments: {} in {:?}",
            reviews.len(),
            start.elapsed()
        );
        trace!(log, "register the actual reviews");
        let start = Instant::now();
        review
            .perform_all(
                reviews
                    .chunks(PAPERS_PER_REVIEWER)
                    .enumerate()
                    .flat_map(|(i, rs)| {
                        rs.iter().map(move |r| {
                            vec![
                                "0".into(),
                                r.paper.into(),
                                format!("{}", i + 1).into(),
                                "review text".into(),
                                r.rating.into(),
                                r.rating.into(),
                                r.rating.into(),
                                r.confidence.into(),
                            ]
                        })
                    }),
            )
            .unwrap();
        println!("# reviews: {} in {:?}", reviews.len(), start.elapsed());
        debug!(log, "population completed");
        memstats(&mut g, "populated");

        if let Some(gloc) = args.value_of("graph") {
            debug!(log, "extracing query graph");
            let gv = g.graphviz().expect("failed to read graphviz");
            std::fs::write(gloc, gv).expect("failed to save graphviz output");
        }

        // for debugging
        println!("{}", g.graphviz().unwrap());        
        g.extend_recipe(
            std::fs::read_to_string("gc_queries.sql")
                .expect("failed to read queries file"),
        )
            .expect("failed to load initial schema");
        thread::sleep(Duration::from_millis(2000));        
        let mut gc_lookup = g.view("GroupContext").unwrap().into_sync();
        let res = gc_lookup.lookup(&[0.into()], true); // bogokey lookup
        println!("GC: {:?}", res);
        //
        
        debug!(log, "logging in users"; "n" => nlogged);
        let mut printi = 0;
        let stripe = nlogged / 10;
        let mut login_times = Vec::with_capacity(nlogged);
        for (i, &uid) in authors.iter().take(nlogged).enumerate() {
            trace!(log, "logging in user"; "uid" => uid);
            let user_context: HashMap<_, _> =
                std::iter::once(("id".to_string(), (i + 1).into())).collect();
            let start = Instant::now();
            g.on_worker(|w| w.create_universe(user_context.clone()))
                .unwrap();
            let took = start.elapsed();
            login_times.push(took);

            if i == printi {
                println!("# login sample[{}]: {:?}", i, login_times[i]);
                if i == 0 {
                    // we want to include both 0 and 1
                    printi += 1;
                } else if i == 1 {
                    // and then go back to every stripe'th sample
                    printi = stripe;
                } else {
                    printi += stripe;
                }
            }
        }

        
        // For debugging: print graph
        println!("{}", g.graphviz().unwrap());
        
        info!(log, "creating api handles");
        debug!(log, "creating view handles for paper list");
        let mut paper_list: HashMap<_, _> = (0..nlogged)
            .map(|uid| {
                trace!(log, "creating posts handle for user"; "uid" => authors[uid]);
                (
                    authors[uid],
                    g.view(format!("ReviewList_u{}", uid + 1))
                        .unwrap()
                        .into_sync(),
                )
            })
            .collect();
        debug!(log, "all api handles created");

        println!("# setup time: {:?}", init.elapsed());

        // now time to measure the cost of different operations
        // for debugging
        //        let mut gc_lookup = g.view("GroupContext_reviewers_3").unwrap().into_sync();
        let mut gc_lookup = g.view("GroupContext").unwrap().into_sync();
        println!("Numeric lookups");
        for i in 0..7 {
            let res = gc_lookup.lookup(&[i.into()], true);
            println!("GC[{}]: {:?}", i, res);
        }
        println!("String lookups");
        for i in 1..7 {
            let res = gc_lookup.lookup(&[format!("{}", i).into()], true);
            println!("GC[{}]: {:?}", i, res);
        }
        //
        info!(log, "starting cold read benchmarks");
        debug!(log, "cold reads of paper list");
        let mut requests = Vec::new();
        let mut i = 1; // for debugging
        'pl_outer: for uid in authors[0..nlogged].choose_multiple(&mut rng, nlogged) {
            trace!(log, "reading paper list"; "uid" => uid);
            requests.push((Operation::ReadPaperList, uid));
            let begin = Instant::now();
            let result = paper_list
                .get_mut(uid)
                .unwrap()
                .lookup(&[0.into(/* bogokey */)], true)
                .unwrap();
            // for debugging
            println!("Reviewer ID {} ({}): {:#?}", uid, i, result);
            i += 1;
            let took = begin.elapsed();

            // NOTE: do we want a warm-up period/drop first sample per uid?
            // trace!(log, "dropping sample during warm-up"; "at" => ?start.elapsed(), "took" => ?took);

            trace!(log, "recording sample"; "took" => ?took);
            cold_stats
                .entry(Operation::ReadPaperList)
                .or_insert_with(|| Histogram::<u64>::new_with_bounds(10, 1_000_000, 4).unwrap())
                .saturating_record(took.as_micros() as u64);
        }

        info!(log, "starting warm read benchmarks");
        for (op, uid) in requests {
            match op {
                Operation::ReadPaperList => {
                    trace!(log, "reading paper list"; "uid" => uid);
                }
            }

            let begin = Instant::now();
            match op {
                Operation::ReadPaperList => {
                    paper_list
                        .get_mut(uid)
                        .unwrap()
                        .lookup(&[0.into(/* bogokey */)], true)
                        .unwrap();
                }
            }
            let took = begin.elapsed();

            // NOTE: no warm-up for "warm" reads

            trace!(log, "recording sample"; "took" => ?took);
            warm_stats
                .entry(op)
                .or_insert_with(|| Histogram::<u64>::new_with_bounds(10, 1_000_000, 4).unwrap())
                .saturating_record(took.as_micros() as u64);
        }

        info!(log, "measuring space overhead");
        // NOTE: we have already done all possible reads, so no need to do "filling" reads
        memstats(&mut g, "end");
    }

    println!("# op\tphase\tpct\ttime");
    for &q in &[50, 95, 99, 100] {
        for &heat in &["cold", "warm"] {
            let stats = match heat {
                "cold" => &cold_stats,
                "warm" => &warm_stats,
                _ => unreachable!(),
            };
            let mut keys: Vec<_> = stats.keys().collect();
            keys.sort();
            for op in keys {
                let stats = &stats[op];
                if q == 100 {
                    println!("{}\t{}\t100\t{:.2}\tµs", op, heat, stats.max());
                } else {
                    println!(
                        "{}\t{}\t{}\t{:.2}\tµs",
                        op,
                        heat,
                        q,
                        stats.value_at_quantile(q as f64 / 100.0)
                    );
                }
            }
        }
    }
}