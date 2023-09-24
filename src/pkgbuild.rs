use crate::{
        git,
        source::{
            self,
            MapByDomain,
        },
        threading::{
            self,
            wait_if_too_busy,
        },
    };
use git2::Oid;
use std::{
        collections::HashMap,
        env,
        ffi::OsString,
        fs::{
            create_dir_all,
            remove_dir_all,
            rename,
        },
        io::Write,
        os::unix::{
            fs::symlink,
            process::CommandExt
        },
        path::{
            PathBuf,
            Path,
        },
        process::{
            Child,
            Command, 
            Stdio
        },
        thread,
        iter::zip,
    };
use tempfile::tempdir;


#[derive(Clone)]
enum Pkgver {
    Plain,
    Func { pkgver: String },
}

#[derive(Clone)]
pub(crate) struct PKGBUILD {
    name: String,
    url: String,
    build: PathBuf,
    git: PathBuf,
    pkgid: String,
    pkgdir: PathBuf,
    commit: git2::Oid,
    pkgver: Pkgver,
    extract: bool,
    sources: Vec<source::Source>,
}

impl source::MapByDomain for PKGBUILD {
    fn url(&self) -> &str {
        self.url.as_str()
    }
}

impl git::ToReposMap for PKGBUILD {
    fn url(&self) -> &str {
        self.url.as_str()
    }

    fn path(&self) -> Option<&Path> {
        Some(&self.git.as_path())
    }
}

fn read_pkgbuilds_yaml<P>(yaml: P) -> Vec<PKGBUILD>
where
    P: AsRef<Path>
{
    let f = std::fs::File::open(yaml)
            .expect("Failed to open pkgbuilds YAML config");
    let config: HashMap<String, String> =
        serde_yaml::from_reader(f)
            .expect("Failed to parse into config");
    let mut pkgbuilds: Vec<PKGBUILD> = config.iter().map(
        |(name, url)| {
            let mut build = PathBuf::from("build");
            build.push(name);
            let git =
                PathBuf::from(format!("sources/PKGBUILD/{}", name));
            PKGBUILD {
                name: name.clone(),
                url: url.clone(),
                build,
                git,
                pkgid: String::new(),
                pkgdir: PathBuf::from("pkgs"),
                commit: Oid::zero(),
                pkgver: Pkgver::Plain,
                extract: false,
                sources: vec![],
            }
    }).collect();
    pkgbuilds.sort_unstable_by(
        |a, b| a.name.cmp(&b.name));
    pkgbuilds
}

fn sync_pkgbuilds(pkgbuilds: &Vec<PKGBUILD>, hold: bool, proxy: Option<&str>) {
    let map =
        PKGBUILD::map_by_domain(pkgbuilds);
    let repos_map =
        git::ToReposMap::to_repos_map(map, "sources/PKGBUILD");
    git::Repo::sync_mt(repos_map, git::Refspecs::MasterOnly, hold, proxy);
}

fn healthy_pkgbuild(pkgbuild: &mut PKGBUILD, set_commit: bool) -> bool {
    let repo =
        match git::Repo::open_bare(&pkgbuild.git, &pkgbuild.url) {
            Some(repo) => repo,
            None => {
                eprintln!("Failed to open or init bare repo {}",
                pkgbuild.git.display());
                return false
            }
        };
    if set_commit {
        match repo.get_branch_commit_id("master") {
            Some(id) => pkgbuild.commit = id,
            None => {
                eprintln!("Failed to set commit id for pkgbuild {}",
                            pkgbuild.name);
                return false
            },
        }
    }
    println!("PKGBUILD '{}' at commit '{}'", pkgbuild.name, pkgbuild.commit);
    match repo.get_pkgbuild_blob() {
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
            git::Repo::open_bare(&pkgbuild.git, &pkgbuild.url)
            .expect("Failed to open repo");
        let blob = repo.get_pkgbuild_blob()
            .expect("Failed to get PKGBUILD blob");
        let mut file =
            std::fs::File::create(path)
            .expect("Failed to create file");
        file.write_all(blob.content()).expect("Failed to write");
    }
}

fn ensure_deps<P: AsRef<Path>> (dir: P, pkgbuilds: &mut Vec<PKGBUILD>) {
    const SCRIPT: &str = include_str!("scripts/get_depends.bash");
    let children: Vec<Child> = pkgbuilds.iter().map(|pkgbuild| {
        let pkgbuild_file = dir.as_ref().join(&pkgbuild.name);
        Command::new("/bin/bash")
            .arg("-ec")
            .arg(SCRIPT)
            .arg("Depends reader")
            .arg(&pkgbuild_file)
            .stdout(Stdio::piped())
            .spawn()
            .expect("Failed to spawn depends reader")
    }).collect();
    let mut deps = vec![];
    for child in children {
        let output = child.wait_with_output()
            .expect("Failed to wait for child");
        for line in 
            output.stdout.split(|byte| byte == &b'\n') 
        {
            if line.len() == 0 {
                continue;
            }
            deps.push(String::from_utf8_lossy(line).into_owned());
        }
    }
    if deps.len() == 0 {
        return
    }
    deps.sort();
    deps.dedup();
    println!("Ensuring {} deps: {:?}", deps.len(), deps);
    let output = Command::new("/usr/bin/pacman")
        .arg("-T")
        .args(&deps)
        .output()
        .expect("Failed to run pacman to get missing deps");
    match output.status.code() {
        Some(code) => match code {
            0 => return,
            127 => (),
            _ => {
                eprintln!(
                    "Pacman returned unexpected {} which marks fatal error",
                    code);
                panic!("Pacman fatal error");
            }
        },
        None => panic!("Failed to get return code from pacman"),
    }
    deps.clear();
    for line in output.stdout.split(|byte| *byte == b'\n') {
        if line.len() == 0 {
            continue;
        }
        deps.push(String::from_utf8_lossy(line).into_owned());
    }
    if deps.len() == 0 {
        return;
    }

    println!("Installing {} missing deps: {:?}", deps.len(), deps);
    let mut child = Command::new("/usr/bin/sudo")
        .arg("/usr/bin/pacman")
        .arg("-S")
        .arg("--noconfirm")
        .args(&deps)
        .spawn()
        .expect("Failed to run sudo pacman to install missing deps");
    let exit_status = child.wait()
        .expect("Failed to wait for child sudo pacman process");
    if let Some(code) = exit_status.code() {
        if code == 0 {
            return
        }
        println!("Failed to run sudo pacman, return: {}", code);
    }
    panic!("Sudo pacman process not successful");
}

fn get_all_sources<P: AsRef<Path>> (dir: P, pkgbuilds: &mut Vec<PKGBUILD>)
    -> (Vec<source::Source>, Vec<source::Source>, Vec<source::Source>) {
    let mut sources_non_unique = vec![];
    for pkgbuild in pkgbuilds.iter_mut() {
        pkgbuild.sources = source::get_sources::<P>(
            &dir.as_ref().join(&pkgbuild.name))
    }
    for pkgbuild in pkgbuilds.iter() {
        for source in pkgbuild.sources.iter() {
            sources_non_unique.push(source);
        }
    }
    source::unique_sources(&sources_non_unique)
}

fn get_pkgbuilds<P>(config: P, hold: bool, noclean: bool, proxy: Option<&str>)
    -> Vec<PKGBUILD>
where
    P:AsRef<Path>
{
    let mut pkgbuilds = read_pkgbuilds_yaml(config);
    let update_pkg = if hold {
        if healthy_pkgbuilds(&mut pkgbuilds, true) {
            println!(
                "Holdpkg set and all PKGBUILDs healthy, no need to update");
            false
        } else {
            eprintln!(
                "Warning: holdpkg set, but PKGBUILDs unhealthy, need update");
            true
        }
    } else {
        true
    };
    // Should not need sort, as it's done when pkgbuilds was read
    let used: Vec<String> = pkgbuilds.iter().map(
        |pkgbuild| pkgbuild.name.clone()).collect();
    let cleaner = match noclean {
        true => None,
        false => Some(thread::spawn(move || 
                    source::remove_unused("sources/PKGBUILD", &used))),
    };
    if update_pkg {
        sync_pkgbuilds(&pkgbuilds, hold, proxy);
        if ! healthy_pkgbuilds(&mut pkgbuilds, true) {
            panic!("Updating broke some of our PKGBUILDs");
        }
    }
    if let Some(cleaner) = cleaner {
        cleaner.join().expect("Failed to join PKGBUILDs cleaner thread");
    }
    pkgbuilds
}

fn extractor_source(pkgbuild: &PKGBUILD) -> Child {
    const SCRIPT: &str = include_str!("scripts/extract_sources.bash");
    create_dir_all(&pkgbuild.build)
        .expect("Failed to create build dir");
    let repo = 
        git::Repo::open_bare(&pkgbuild.git, &pkgbuild.url)
        .expect("Failed to open repo");
    repo.checkout_branch(&pkgbuild.build, "master");
    source::extract(&pkgbuild.build, &pkgbuild.sources);
    let mut arg0 = OsString::from("[EXTRACTOR/");
    arg0.push(&pkgbuild.name);
    arg0.push("] /bin/bash");
    Command::new("/bin/bash")
        .arg0(&arg0)
        .arg("-ec")
        .arg(SCRIPT)
        .arg("Source extractor")
        .arg(&pkgbuild.build.canonicalize()
            .expect("Failed to cannicalize build dir"))
        .spawn()
        .expect("Failed to run script")
}

fn extract_sources(pkgbuilds: &mut [&mut PKGBUILD]) {
    let children: Vec<Child> = pkgbuilds.iter_mut().map(
    |pkgbuild| 
    {
        pkgbuild.extract = true;
        extractor_source(pkgbuild)
    }).collect();
    for mut child in children {
        child.wait().expect("Failed to wait for child");
    }
}

fn fill_all_pkgvers<P: AsRef<Path>>(dir: P, pkgbuilds: &mut Vec<PKGBUILD>) {
    let _ = remove_dir_all("build");
    let dir = dir.as_ref();
    let children: Vec<Child> = pkgbuilds.iter().map(|pkgbuild| 
        Command::new("/bin/bash")
            .arg("-c")
            .arg(". \"$1\"; type -t pkgver")
            .arg("Type Identifier")
            .arg(dir.join(&pkgbuild.name))
            .stdout(Stdio::piped())
            .spawn()
            .expect("Failed to run script")
    ).collect();
    let mut pkgbuilds_with_pkgver_func = vec![];
    for (child, pkgbuild) in 
        zip(children, pkgbuilds.iter_mut()) 
    {
        let output = child.wait_with_output()
            .expect("Failed to wait for spanwed script");
        if output.stdout.as_slice() ==  b"function\n" {
            pkgbuilds_with_pkgver_func.push(pkgbuild);
        };
    }
    extract_sources(&mut pkgbuilds_with_pkgver_func);
    let children: Vec<Child> = pkgbuilds_with_pkgver_func.iter().map(
    |pkgbuild|
        Command::new("/bin/bash")
            .arg("-ec")
            .arg("srcdir=\"$1\"; cd \"$1\"; source ../PKGBUILD; pkgver")
            .arg("Pkgver runner")
            .arg(pkgbuild.build.join("src")
                .canonicalize()
                .expect("Failed to canonicalize dir"))
            .stdout(Stdio::piped())
            .spawn()
            .expect("Failed to run script")
    ).collect();
    for (child, pkgbuild) in 
        zip(children, pkgbuilds_with_pkgver_func.iter_mut()) 
    {
        let output = child.wait_with_output()
            .expect("Failed to wait for child");
        pkgbuild.pkgver = Pkgver::Func { pkgver:
            String::from_utf8_lossy(&output.stdout).trim().to_string()}
    }
}

fn fill_all_pkgdirs(pkgbuilds: &mut Vec<PKGBUILD>) {
    for pkgbuild in pkgbuilds.iter_mut() {
        let mut pkgid = format!(
            "{}-{}", pkgbuild.name, pkgbuild.commit);
        if let Pkgver::Func { pkgver } = &pkgbuild.pkgver {
            pkgid.push('-');
            pkgid.push_str(&pkgver);
        }
        pkgbuild.pkgdir.push(&pkgid);
        pkgbuild.pkgid = pkgid;
        println!("Pkgdir for '{}': '{}'",
            pkgbuild.name, pkgbuild.pkgdir.display());
    }
}

fn extract_if_need_build(pkgbuilds: &mut Vec<PKGBUILD>) {
    let mut pkgbuilds_need_build = vec![];
    let mut cleaners = vec![];
    for pkgbuild in pkgbuilds.iter_mut() {
        let mut built = false;
        if let Ok(mut dir) = pkgbuild.pkgdir.read_dir() {
            if let Some(_) = dir.next() {
                built = true;
            }
        }
        if built { // Does not need build
            println!("'{}' already built, no need to build",
                pkgbuild.pkgdir.display());
            if pkgbuild.extract {
                let dir = pkgbuild.build.clone();
                wait_if_too_busy(&mut cleaners, 30, 
                    "cleaning builddir");
                cleaners.push(thread::spawn(||
                    remove_dir_all(dir)
                    .expect("Failed to remove dir")));
                pkgbuild.extract = false;
            }
        } else {
            if ! pkgbuild.extract {
                pkgbuild.extract = true;
                pkgbuilds_need_build.push(pkgbuild);
            }
        }
    }
    extract_sources(&mut pkgbuilds_need_build);
    threading::wait_remaining(cleaners, "cleaning builddirs");
}

fn prepare_sources<P: AsRef<Path>>(
    dir: P,
    pkgbuilds: &mut Vec<PKGBUILD>,
    holdgit: bool,
    skipint: bool,
    noclean: bool,
    proxy: Option<&str>
) {
    let build = PathBuf::from("build");
    let cleaner = match build.exists() {
        true => Some(thread::spawn(|| remove_dir_all("build"))),
        false => None,
    };
    dump_pkgbuilds(&dir, pkgbuilds);
    ensure_deps(&dir, pkgbuilds);
    let (netfile_sources, git_sources, _)
        = get_all_sources(&dir, pkgbuilds);
    source::cache_sources_mt(
        &netfile_sources, &git_sources, holdgit, skipint, proxy);
    if let Some(cleaner) = cleaner {
        match cleaner.join()
            .expect("Failed to join build dir cleaner thread") {
            Ok(_) => (),
            Err(e) => {
                eprintln!("Failed to clean build dir: {}", e);
                panic!("Failed to clean build dir");
            },
        }
    }
    let cleaners = match noclean {
        true => None,
        false => Some(source::cleanup(netfile_sources, git_sources)),
    };
    fill_all_pkgvers(dir, pkgbuilds);
    fill_all_pkgdirs(pkgbuilds);
    extract_if_need_build(pkgbuilds);
    if let Some(cleaners) = cleaners {
        for cleaner in cleaners {
            cleaner.join().expect("Failed to join sources cleaner thread");
        }
    }
}

fn build(pkgbuild: &PKGBUILD, nonet: bool) {
    let mut temp_name = pkgbuild.pkgdir.file_name()
        .expect("Failed to get file name").to_os_string();
    temp_name.push(".temp");
    let temp_pkgdir = pkgbuild.pkgdir.with_file_name(temp_name);
    let _ = create_dir_all(&temp_pkgdir);
    let mut command = if nonet {
        let mut command = Command::new("/usr/bin/unshare");
        command.arg("--map-root-user")
            .arg("--net")
            .arg("--")
            .arg("sh")
            .arg("-c")
            .arg(format!(
                "ip link set dev lo up
                unshare --map-users={}:0:1 --map-groups={}:0:1 -- \
                    makepkg --holdver --nodeps --noextract --ignorearch", 
                unsafe {libc::getuid()}, unsafe {libc::getgid()}));
        command
    } else {
        let mut command = Command::new("/bin/bash");
        command.arg("/usr/bin/makepkg")
            .arg("--holdver")
            .arg("--nodeps")
            .arg("--noextract")
            .arg("--ignorearch");
        command
    };
    command.current_dir(&pkgbuild.build)
        .arg0(format!("[BUILDER/{}] /bin/bash", pkgbuild.pkgid))
        .env("PATH",
            env::var_os("PATH")
            .expect("Failed to get PATH env"))
        .env("HOME",
            env::var_os("HOME")
            .expect("Failed to get HOME env"))
        .env("PKGDEST",
            &temp_pkgdir.canonicalize()
            .expect("Failed to get absolute path of pkgdir"));
    for i in 1..3 {
        println!("Building '{}', try {}/{}", &pkgbuild.pkgid, i , 3);
        let _ = create_dir_all(&temp_pkgdir);
        let exit_status = command
            .spawn()
            .expect("Failed to spawn makepkg")
            .wait()
            .expect("Failed to wait for makepkg");
        match exit_status.code() {
            Some(0) => {
                println!("Successfully built '{}'", temp_pkgdir.display());
                break
            },
            _ => {
                eprintln!("Failed to build '{}'", temp_pkgdir.display());
                let _ = remove_dir_all(&pkgbuild.pkgdir);
                let _ = remove_dir_all(&temp_pkgdir);
                if i == 3 {
                    eprintln!("Failed to build '{}' after all tries",
                            temp_pkgdir.display());
                    return
                }
                let _ = remove_dir_all(&pkgbuild.build);
                extractor_source(&pkgbuild).wait()
                    .expect("Failed re-extract source");
            }
        }
    }
    println!("Finishing building '{}'", &pkgbuild.pkgid);
    let build = pkgbuild.build.clone();
    let thread_cleaner =
        thread::spawn(|| remove_dir_all(build));
    let _ = remove_dir_all(&pkgbuild.pkgdir);
    rename(&temp_pkgdir, &pkgbuild.pkgdir)
        .expect("Failed to move result pkgdir");
    let mut rel = PathBuf::from("..");
    rel.push(&pkgbuild.pkgid);
    let updated = PathBuf::from("pkgs/updated");
    for entry in
        pkgbuild.pkgdir.read_dir().expect("Failed to read dir")
    {
        if let Ok(entry) = entry {
            let original = rel.join(entry.file_name());
            let link = updated.join(entry.file_name());
            let _ = symlink(original, link);
        }
    }
    let _ = thread_cleaner.join().expect("Failed to join cleaner thread");
    println!("Finished building '{}'", &pkgbuild.pkgid);
}

fn build_any_needed(pkgbuilds: &Vec<PKGBUILD>, nonet: bool) {
    let _ = remove_dir_all("pkgs/updated");
    let _ = remove_dir_all("pkgs/latest");
    let _ = create_dir_all("pkgs/updated");
    let _ = create_dir_all("pkgs/latest");
    let mut threads = vec![];
    for pkgbuild in pkgbuilds.iter() {
        if ! pkgbuild.extract {
            continue
        }
        let pkgbuild = pkgbuild.clone();
        wait_if_too_busy(&mut threads, 5, "building packages");
        threads.push(thread::spawn(move || build(&pkgbuild, nonet)));
    }
    threading::wait_remaining(threads, "building packages");
    let thread_cleaner =
        thread::spawn(|| remove_dir_all("build"));
    let rel = PathBuf::from("..");
    let latest = PathBuf::from("pkgs/latest");
    for pkgbuild in pkgbuilds.iter() {
        let rel = rel.join(&pkgbuild.pkgid);
        for entry in
            pkgbuild.pkgdir.read_dir().expect("Failed to read dir")
        {
            if let Ok(entry) = entry {
                let original = rel.join(entry.file_name());
                let link = latest.join(entry.file_name());
                let _ = symlink(original, link);
            }
        }
    }
    let _ = thread_cleaner.join().expect("Failed to join cleaner thread");
}

fn clean_pkgdir(pkgbuilds: &Vec<PKGBUILD>) {
    let mut used: Vec<String> = pkgbuilds.iter().map(
        |pkgbuild| pkgbuild.pkgid.clone()).collect();
    used.push(String::from("updated"));
    used.push(String::from("latest"));
    used.sort_unstable();
    source::remove_unused("pkgs", &used);
}

pub(crate) fn work<P: AsRef<Path>>(
    pkgbuilds_yaml: P,
    proxy: Option<&str>,
    holdpkg: bool,
    holdgit: bool,
    skipint: bool,
    nobuild: bool,
    noclean: bool,
    nonet: bool,
) {
    let mut pkgbuilds =
        get_pkgbuilds(
            &pkgbuilds_yaml, holdpkg, noclean, proxy);
    let pkgbuilds_dir =
        tempdir().expect("Failed to create temp dir to dump PKGBUILDs");
    prepare_sources(
        pkgbuilds_dir, &mut pkgbuilds, holdgit, skipint, noclean, proxy);
    if nobuild {
        return;
    }
    build_any_needed(&pkgbuilds, nonet);
    if noclean {
        return;
    }
    clean_pkgdir(&pkgbuilds);
}