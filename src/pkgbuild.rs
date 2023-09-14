use std::{path::{PathBuf, Path}, collections::{BTreeMap, HashMap}, thread::{self, sleep, JoinHandle}, io::Write, process::Command, fs::{DirBuilder, remove_dir_all, create_dir_all}, time::Duration};

use git2::{Repository, Oid};
use hex::ToHex;
use url::Url;
use xxhash_rust::xxh3::xxh3_64;
use crate::{git, source, threading::{self, wait_if_too_busy}};

#[derive(Clone)]
enum Pkgver {
    Plain,
    Func { pkgver: String },
}

#[derive(Clone)]
pub(crate) struct PKGBUILD {
    name: String,
    url: String,
    hash_url: u64,
    hash_domain: u64,
    build: PathBuf,
    git: PathBuf,
    pkgdir: PathBuf,
    commit: git2::Oid,
    pkgver: Pkgver,
    extract: bool,
    sources: Vec<source::Source>,
}

struct Repo {
    path: PathBuf,
    url: String,
}

fn read_pkgbuilds_yaml<P>(yaml: P) -> Vec<PKGBUILD>
where 
    P: AsRef<Path>
{
    let f = std::fs::File::open(yaml)
            .expect("Failed to open pkgbuilds YAML config");
    let config: BTreeMap<String, String> = 
        serde_yaml::from_reader(f)
            .expect("Failed to parse into config");
    config.iter().map(|(name, url)| {
        let url_p = Url::parse(url).expect("Invalid URL");
        let hash_domain = match url_p.domain() {
            Some(domain) => xxh3_64(domain.as_bytes()),
            None => 0,
        };
        let hash_url = xxh3_64(url.as_bytes());
        let mut build = PathBuf::from("build");
        build.push(name);
        let git = PathBuf::from(format!("sources/PKGBUILDs/{}", name));
        PKGBUILD {
            name: name.clone(),
            url: url.clone(),
            hash_url,
            hash_domain,
            build,
            git,
            pkgdir: PathBuf::from("pkgs"),
            commit: Oid::zero(),
            pkgver: Pkgver::Plain,
            extract: false,
            sources: vec![],
        }
    }).collect()
}

fn sync_pkgbuilds(pkgbuilds: &Vec<PKGBUILD>, proxy: Option<&str>) {
    let mut map: HashMap<u64, Vec<Repo>> = HashMap::new();
    for pkgbuild in pkgbuilds.iter() {
        if ! map.contains_key(&pkgbuild.hash_domain) {
            println!("New domain found from PKGBUILD URL: {}", pkgbuild.url);
            map.insert(pkgbuild.hash_domain, vec![]);
        }
        let vec = map
            .get_mut(&pkgbuild.hash_domain)
            .expect("Failed to get vec");
        vec.push(Repo { path: pkgbuild.git.clone(), url: pkgbuild.url.clone() });
    }
    println!("Syncing PKGBUILDs with {} threads", map.len());
    const REFSPECS: &[&str] = &["+refs/heads/master:refs/heads/master"];
    let (proxy_string, has_proxy) = match proxy {
        Some(proxy) => (proxy.to_owned(), true),
        None => (String::new(), false),
    };
    let mut threads =  Vec::new();
    for repos in map.into_values() {
        let proxy_string_thread = proxy_string.clone();
        threads.push(thread::spawn(move || {
            let proxy = match has_proxy {
                true => Some(proxy_string_thread.as_str()),
                false => None,
            };
            for repo in repos {
                git::sync_repo(&repo.path, &repo.url, proxy, REFSPECS);
            }
        }));
    }
    for thread in threads.into_iter() {
        thread.join().expect("Failed to join");
    }
}

fn get_pkgbuild_blob(repo: &Repository) -> Option<git2::Blob> {
    git::get_branch_entry_blob(repo, "master", "PKGBUILD")
}

fn healthy_pkgbuild(pkgbuild: &mut PKGBUILD, set_commit: bool) -> bool {
    let repo = 
        match git::open_or_init_bare_repo(&pkgbuild.git, &pkgbuild.url) {
            Some(repo) => repo,
            None => {
                eprintln!("Failed to open or init bare repo {}", pkgbuild.git.display());
                return false
            }
        };
    if set_commit {
        match git::get_branch_commit_id(&repo, "master") {
            Some(id) => pkgbuild.commit = id,
            None => {
                eprintln!("Failed to set commit id for pkgbuild {}", pkgbuild.name);
                return false
            },
        }
    }
    println!("PKGBUILD '{}' at commit '{}'", pkgbuild.name, pkgbuild.commit);
    match get_pkgbuild_blob(&repo) {
        Some(_) => return true,
        None => {
            eprintln!("Failed to get PKGBUILD blob");
            return false
        },
    };
}

fn healthy_pkgbuilds(pkgbuilds: &mut Vec<PKGBUILD>, set_commit: bool) -> bool {
    for pkgbuild in pkgbuilds.iter_mut() {
        if ! healthy_pkgbuild(pkgbuild, set_commit) {
            return false;
        }
    }
    true
}

fn dump_pkgbuilds<P> (dir: P, pkgbuilds: &Vec<PKGBUILD>)
where 
    P: AsRef<Path> 
{
    let dir = dir.as_ref();
    for pkgbuild in pkgbuilds.iter() {
        let path = dir.join(&pkgbuild.name);
        let repo = 
            git::open_or_init_bare_repo(&pkgbuild.git, &pkgbuild.url)
            .expect("Failed to open repo");
        let blob = 
            get_pkgbuild_blob(&repo)
            .expect("Failed to get PKGBUILD blob");
        let mut file = 
            std::fs::File::create(path)
            .expect("Failed to create file");
        file.write_all(blob.content()).expect("Failed to write");
    }
}

fn get_all_sources<P> (dir: P, pkgbuilds: &mut Vec<PKGBUILD>) 
    -> (Vec<source::Source>, Vec<source::Source>, Vec<source::Source>)
where 
    P: AsRef<Path> 
{
    let mut sources_non_unique = vec![];
    for pkgbuild in pkgbuilds.iter_mut() {
        pkgbuild.sources = source::get_sources::<P>(&dir.as_ref().join(&pkgbuild.name))
    }
    for pkgbuild in pkgbuilds.iter() {
        for source in pkgbuild.sources.iter() {
            sources_non_unique.push(source);
        }
    }
    source::unique_sources(&sources_non_unique)
}

pub(crate) fn get_pkgbuilds<P>(config: P, hold: bool, proxy: Option<&str>) -> Vec<PKGBUILD>
where 
    P:AsRef<Path>
{
    let mut pkgbuilds = read_pkgbuilds_yaml(config);
    let update_pkg = if hold {
        if healthy_pkgbuilds(&mut pkgbuilds, true) {
            println!("Holdpkg set and all PKGBUILDs healthy, no need to update");
            false
        } else {
            eprintln!("Warning: holdpkg set, but unhealthy PKGBUILDs found, still need to update");
            true
        }
    } else {
        true
    };
    if update_pkg {
        sync_pkgbuilds(&pkgbuilds, proxy);
        if ! healthy_pkgbuilds(&mut pkgbuilds, true) {
            panic!("Updating broke some of our PKGBUILDs");
        }
    }
    pkgbuilds
}

fn extract_source<P: AsRef<Path>>(dir: P, repo: P, sources: &Vec<source::Source>) {
    create_dir_all(&dir).expect("Failed to create dir");
    git::checkout_branch_from_repo(&dir, &repo, "master");
    source::extract(&dir, sources);
    const SCRIPT: &str = include_str!("scripts/extract_sources.bash");
    Command::new("/bin/bash")
        .arg("-ec")
        .arg(SCRIPT)
        .arg("Source reader")
        .arg(dir.as_ref().canonicalize().expect("Failed to canonicalize dir"))
        .spawn()
        .expect("Failed to run script")
        .wait()
        .expect("Failed to wait for spawned script");
}

fn extract_source_and_get_pkgver<P: AsRef<Path>>(dir: P, repo: P, sources: &Vec<source::Source>) -> String {
    extract_source(&dir, &repo, sources);
    let output = Command::new("/bin/bash")
        .arg("-ec")
        .arg("cd $1; source ../PKGBUILD; pkgver")
        .arg("Source reader")
        .arg(dir.as_ref().join("src").canonicalize().expect("Failed to canonicalize dir"))
        .output()
        .expect("Failed to run script");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn fill_all_pkgvers<P: AsRef<Path>>(dir: P, pkgbuilds: &mut Vec<PKGBUILD>) {
    let _ = remove_dir_all("build");
    let dir = dir.as_ref();
    for pkgbuild in pkgbuilds.iter_mut() {
        let output = Command::new("/bin/bash")
            .arg("-c")
            .arg(". \"$1\"; type -t pkgver")
            .arg("Type Identifier")
            .arg(dir.join(&pkgbuild.name))
            .output()
            .expect("Failed to run script");
        pkgbuild.extract = match output.stdout.as_slice() {
            b"function\n" => true,
            _ => false,
        }
    }
    let mut dir_builder = DirBuilder::new();
    dir_builder.recursive(true);
    struct PkgbuildThread<'a> {
        pkgbuild: &'a mut PKGBUILD,
        thread: JoinHandle<String>
    }
    let mut pkgbuild_threads: Vec<PkgbuildThread> = vec![];
    for pkgbuild in pkgbuilds.iter_mut().filter(|pkgbuild| pkgbuild.extract) {
        let dir = pkgbuild.build.clone();
        let repo = pkgbuild.git.clone();
        let sources = pkgbuild.sources.clone();
        let mut thread_id_finished = None;
        if pkgbuild_threads.len() > 20 {
            loop {
                for (thread_id, pkgbuild_thread) in pkgbuild_threads.iter().enumerate() {
                    if pkgbuild_thread.thread.is_finished() {
                        thread_id_finished = Some(thread_id);
                        break;
                    }
                }
                if let None = thread_id_finished {
                    sleep(Duration::from_millis(10));
                } else {
                    break
                }
            }
            if let Some(thread_id_finished) = thread_id_finished {
                let pkgbuild_thread = pkgbuild_threads.swap_remove(thread_id_finished);
                let pkgver = pkgbuild_thread.thread.join().expect("Failed to join finished thread");
                pkgbuild_thread.pkgbuild.pkgver = Pkgver::Func { pkgver };
            } else {
                panic!("Failed to get finished thread ID")
            }
        }
        pkgbuild_threads.push(PkgbuildThread { pkgbuild, thread: thread::spawn(move || extract_source_and_get_pkgver(dir, repo, &sources))});
    }
    for pkgbuild_thread in pkgbuild_threads {
        let pkgver = pkgbuild_thread.thread.join().expect("Failed to join finished thread");
        pkgbuild_thread.pkgbuild.pkgver = Pkgver::Func { pkgver };
    }
}

fn fill_all_pkgdirs(pkgbuilds: &mut Vec<PKGBUILD>) {
    for pkgbuild in pkgbuilds.iter_mut() {
        let mut name = format!("{}-{}", pkgbuild.name, pkgbuild.commit);
        if let Pkgver::Func { pkgver } = &pkgbuild.pkgver {
            name.push('-');
            name.push_str(&pkgver);
        }
        pkgbuild.pkgdir.push(&name);
        println!("PKGDIR: '{}' -> '{}'", pkgbuild.name, pkgbuild.pkgdir.display());
    }
}

fn extract_if_need_build(pkgbuilds: &mut Vec<PKGBUILD>) {
    let mut threads = vec![];
    for pkgbuild in pkgbuilds.iter_mut() {
        let mut built = false;
        if let Ok(mut dir) = pkgbuild.pkgdir.read_dir() {
            if let Some(_) = dir.next() {
                built = true;
            }
        }
        if built { // Does not need build
            println!("'{}' already built, no need to build", pkgbuild.pkgdir.display());
            if pkgbuild.extract {
                let dir = pkgbuild.build.clone();
                wait_if_too_busy(&mut threads, 20);
                threads.push(thread::spawn(|| remove_dir_all(dir).expect("Failed to remove dir")));
                pkgbuild.extract = false;
            }
        } else {
            if ! pkgbuild.extract {
                let dir = pkgbuild.build.clone();
                let repo = pkgbuild.git.clone();
                let sources = pkgbuild.sources.clone();
                wait_if_too_busy(&mut threads, 20);
                threads.push(thread::spawn(move || extract_source(dir, repo, &sources)));
                pkgbuild.extract = true;
            }
        }
    }
    for thread in threads {
        thread.join().expect("Failed to join finished thread");
    }
}

pub(crate) fn prepare_sources<P: AsRef<Path>>(dir: P, pkgbuilds: &mut Vec<PKGBUILD>, holdgit: bool, skipint: bool, proxy: Option<&str>) {
    dump_pkgbuilds(&dir, pkgbuilds);
    let (netfile_sources, git_sources, local_sources) 
        = get_all_sources(&dir, pkgbuilds);
    source::cache_sources_mt(&netfile_sources, &git_sources, holdgit, skipint, proxy);
    fill_all_pkgvers(dir, pkgbuilds);
    fill_all_pkgdirs(pkgbuilds);
    extract_if_need_build(pkgbuilds);
}