//! `capi`: differential tests of the Lua C API. Each program in
//! `tests/capi` (`NAME.c`) is built against orbit-capi's headers and linked
//! to the laid-out liborbit, run, and its output compared with
//! `tests/capi/expected-X.Y/NAME.txt`: what the same program prints linked to
//! the reference liblua. On Linux, `--reference` also builds and runs each
//! program on the reference library and checks that the committed output is
//! still what it prints, so the expectations are never hand-written; on
//! macOS and Windows, which have no reference library, the committed output
//! is what liborbit must match.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::conformance;
use crate::process::{self, Exit, Spec};
use crate::util::{self, Result};

const USAGE: &str = "\
usage: orbit-testkit capi [--lua X.Y] [--layout DIR] [--reference LIB] [--update]
                          [--check-linked] [--wrap CMD] [NAME...]

Builds each C program in tests/capi (or only NAME.c) against orbit-capi's
headers and liborbit, runs it, and compares its output with
tests/capi/expected-X.Y/NAME.txt, what the reference liblua prints.

options:
  --lua X.Y        the build's Lua version, 5.5 (default) or 5.4 (ADR-14)
  --layout DIR     the layout ./build.sh made (default target/package/out, or
                   target/package-lua5.4/out with --lua 5.4)
  --reference LIB  also run each program on the reference shared library
                   (Linux: target/reference-lua/shared/liblua5.5.so.0, from
                   scripts/build-reference-lua.sh) and check the committed
                   output against it
  --update         with --reference: write the reference's output as the
                   committed output
  --check-linked   (Linux, needs nm) every function liborbit exports for C
                   must be linked by a program here or be listed, with a
                   reason, in tests/capi/not-linked.txt
  --wrap CMD       run the programs on liborbit under CMD, a command and its
                   arguments split at spaces (tests/memcheck/run.sh runs
                   valgrind so)";

/// How long one program may run, valgrind's slowdown included.
const TIMEOUT: Duration = Duration::from_secs(300);

struct Options {
    lua: String,
    layout: PathBuf,
    reference: Option<PathBuf>,
    update: bool,
    check_linked: bool,
    wrap: Vec<String>,
}

pub fn main(args: &[String]) -> Result<bool> {
    let root = util::repo_root();
    let mut lua = conformance::VERSIONS[0].to_string();
    let mut layout = None;
    let mut reference = None;
    let mut update = false;
    let mut check_linked = false;
    let mut wrap = Vec::new();
    let mut only = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{a} needs a value\n\n{USAGE}"))
        };
        match a.as_str() {
            "--lua" => lua = val()?,
            "--layout" => layout = Some(util::absolute(Path::new(&val()?))?),
            "--reference" => reference = Some(util::absolute(Path::new(&val()?))?),
            "--update" => update = true,
            "--check-linked" => check_linked = true,
            "--wrap" => wrap = val()?.split_whitespace().map(str::to_owned).collect(),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(true);
            }
            s if s.starts_with('-') => return Err(format!("unknown option {s}\n\n{USAGE}")),
            s => only.push(s.trim_end_matches(".c").to_string()),
        }
    }
    if !conformance::VERSIONS.contains(&lua.as_str()) {
        return Err(format!("--lua must be one of {:?}", conformance::VERSIONS));
    }
    if update && reference.is_none() {
        return Err("--update needs --reference".into());
    }
    let layout = layout.unwrap_or_else(|| {
        let package = if lua == conformance::VERSIONS[0] {
            "package".to_string()
        } else {
            format!("package-lua{lua}")
        };
        root.join(format!("target/{package}/out"))
    });
    let opts = Options {
        lua,
        layout,
        reference,
        update,
        check_linked,
        wrap,
    };

    let dir = root.join("tests/capi");
    let mut programs = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(util::io_err(dir.display()))? {
        let path = entry.map_err(util::io_err(dir.display()))?.path();
        if path.extension().is_some_and(|e| e == "c") {
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            if only.is_empty() || only.contains(&name) {
                programs.push(name);
            }
        }
    }
    programs.sort();
    if programs.is_empty() {
        return Err("no programs to run".into());
    }
    if opts.check_linked && !only.is_empty() {
        return Err("--check-linked needs every program: name none".into());
    }

    let liborbit = Library::orbit(&opts.layout, &opts.lua)?;
    let reference = opts
        .reference
        .as_deref()
        .map(Library::reference)
        .transpose()?;
    let work = root.join("target/testkit/capi");
    util::fresh_dir(&work)?;
    let expected_dir = dir.join(format!("expected-{}", opts.lua));
    std::fs::create_dir_all(&expected_dir).map_err(util::io_err(expected_dir.display()))?;

    let mut failed = 0;
    let mut built = Vec::new();
    for name in &programs {
        let source = dir.join(format!("{name}.c"));
        let expected_path = expected_dir.join(format!("{name}.txt"));
        let mut problems = Vec::new();

        if let Some(reference) = &reference {
            let exe = build(reference, &source, &work.join("reference"), name, &opts.lua)?;
            let out = run(reference, &exe, &[])?;
            if opts.update {
                std::fs::write(&expected_path, &out)
                    .map_err(util::io_err(expected_path.display()))?;
            } else {
                match std::fs::read(&expected_path) {
                    Ok(want) if want == out => {}
                    Ok(want) => problems.push(format!(
                        "the reference no longer prints {} (update it with --update):\n{}",
                        rel(&expected_path),
                        first_difference(&want, &out)
                    )),
                    Err(_) => problems.push(format!(
                        "no {} (write it with --reference ... --update)",
                        rel(&expected_path)
                    )),
                }
            }
        }

        let exe = build(&liborbit, &source, &work.join("orbit"), name, &opts.lua)?;
        let out = run(&liborbit, &exe, &opts.wrap)?;
        let _ = std::fs::write(work.join(format!("{name}.orbit.txt")), &out);
        match std::fs::read(&expected_path) {
            Ok(want) if want == out => {}
            Ok(want) => problems.push(format!(
                "liborbit differs from the reference:\n{}",
                first_difference(&want, &out)
            )),
            Err(_) if opts.update => {}
            Err(_) => problems.push(format!("no {}", rel(&expected_path))),
        }
        built.push(exe);

        if problems.is_empty() {
            println!("  pass {name}");
        } else {
            failed += 1;
            println!("  FAIL {name}");
            for p in problems {
                println!("{}", util::indent(&p, "       "));
            }
        }
    }
    println!(
        "\n{} of {} programs match the reference",
        programs.len() - failed,
        programs.len()
    );

    let mut ok = failed == 0;
    if opts.check_linked {
        ok &= linked_check(&root, &liborbit, &built)?;
    }
    Ok(ok)
}

/// The library a program links: liborbit from a layout or the reference.
struct Library {
    /// What the compiler links against: the shared library, or on Windows
    /// its import library.
    link: PathBuf,
    /// The directory the loader must search when the program runs.
    load_dir: PathBuf,
    /// The shared library itself, for `--check-linked`.
    file: PathBuf,
}

impl Library {
    fn orbit(layout: &Path, lua: &str) -> Result<Library> {
        let lib = layout.join("lib");
        let (link, load_dir, file) = if cfg!(windows) {
            let import = lib.join(format!("lua{}.lib", lua.replace('.', "")));
            let bin = layout.join("bin");
            (import, bin.clone(), bin.join("orbit_lib.dll"))
        } else if cfg!(target_os = "macos") {
            let f = lib.join("liborbit.0.dylib");
            (f.clone(), lib, f)
        } else {
            let f = lib.join("liborbit.so.0");
            (f.clone(), lib, f)
        };
        if !link.is_file() {
            return Err(format!(
                "no {} (lay out the library with ./build.sh, or name the layout with --layout)",
                link.display()
            ));
        }
        Ok(Library {
            link,
            load_dir,
            file,
        })
    }

    fn reference(lib: &Path) -> Result<Library> {
        if !cfg!(target_os = "linux") {
            return Err("--reference runs on Linux only".into());
        }
        if !lib.is_file() {
            return Err(format!(
                "no reference library {} (scripts/build-reference-lua.sh builds it)",
                lib.display()
            ));
        }
        Ok(Library {
            link: lib.to_path_buf(),
            load_dir: lib.parent().unwrap().to_path_buf(),
            file: lib.to_path_buf(),
        })
    }
}

fn rel(p: &Path) -> String {
    p.strip_prefix(util::repo_root())
        .unwrap_or(p)
        .display()
        .to_string()
}

/// Builds `source` linked to `lib` into `dir`, returning the program.
fn build(lib: &Library, source: &Path, dir: &Path, name: &str, lua: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).map_err(util::io_err(dir.display()))?;
    let root = util::repo_root();
    let include = root.join("crates/orbit-capi/csrc/include");
    let versioned = include.join(lua);
    let own = root.join("tests/capi");
    let exe = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let mut cmd = compile_command(dir, &[&versioned, &include, &own], source, &exe, lib)?;
    let out = cmd
        .output()
        .map_err(util::io_err(format!("running the C compiler for {name}.c")))?;
    if !out.status.success() {
        return Err(format!(
            "building {name}.c failed:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(exe)
}

#[cfg(unix)]
fn compile_command(
    _dir: &Path,
    includes: &[&Path],
    source: &Path,
    exe: &Path,
    lib: &Library,
) -> Result<Command> {
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let mut cmd = Command::new(cc);
    cmd.args(["-O1", "-g", "-std=c99", "-Wall", "-Wextra", "-Werror"]);
    for i in includes {
        cmd.arg("-I").arg(i);
    }
    cmd.arg("-o").arg(exe).arg(source).arg(&lib.link);
    if cfg!(target_os = "macos") {
        let mut rpath = OsString::from("-Wl,-rpath,");
        rpath.push(&lib.load_dir);
        cmd.arg(rpath);
    }
    Ok(cmd)
}

/// On Windows: MSVC's `cl` (found as the `cc` crate finds it), or MinGW's
/// `gcc` in a GNU toolchain, linked against the layout's import library.
#[cfg(windows)]
fn compile_command(
    dir: &Path,
    includes: &[&Path],
    source: &Path,
    exe: &Path,
    lib: &Library,
) -> Result<Command> {
    if cfg!(target_env = "msvc") {
        let mut cmd = cc::windows_registry::find(std::env::consts::ARCH, "cl.exe")
            .ok_or("MSVC's cl.exe, to build the programs, was not found")?;
        let mut objects = dir.as_os_str().to_owned();
        objects.push("\\");
        let mut fo = OsString::from("/Fo");
        fo.push(&objects);
        let mut fe = OsString::from("/Fe");
        fe.push(exe);
        // The programs use standard C (`fopen`), which MSVC would rather
        // see replaced by its own `_s` functions. `/MD`: the C library as a
        // DLL, the one liborbit uses, as an embedder links it, so the two
        // share `errno` (`luaL_fileresult`); `cl` links its own copy into
        // the program otherwise.
        cmd.args([
            "/nologo",
            "/O1",
            "/W3",
            "/WX",
            "/MD",
            "/D_CRT_SECURE_NO_WARNINGS",
        ]);
        for i in includes {
            cmd.arg("/I").arg(i);
        }
        cmd.arg(fo).arg(fe).arg(source).arg(&lib.link);
        Ok(cmd)
    } else {
        let cc = std::env::var("CC").unwrap_or_else(|_| "gcc".into());
        let mut cmd = Command::new(cc);
        cmd.args(["-O1", "-std=c99", "-Wall", "-Wextra", "-Werror"]);
        for i in includes {
            cmd.arg("-I").arg(i);
        }
        cmd.arg("-o").arg(exe).arg(source).arg(&lib.link);
        Ok(cmd)
    }
}

/// Runs `exe` (under `wrap`, if any) with the loader finding `lib`, and
/// returns what it printed, followed by how it ended when it did not exit
/// with 0.
fn run(lib: &Library, exe: &Path, wrap: &[String]) -> Result<Vec<u8>> {
    let (var, sep) = if cfg!(windows) {
        ("PATH", ";")
    } else if cfg!(target_os = "macos") {
        ("DYLD_LIBRARY_PATH", ":")
    } else {
        ("LD_LIBRARY_PATH", ":")
    };
    let mut path = lib.load_dir.clone().into_os_string();
    if let Some(old) = std::env::var_os(var) {
        path.push(sep);
        path.push(old);
    }
    let (program, args) = match wrap.split_first() {
        Some((first, rest)) => {
            let mut args: Vec<OsString> = rest.iter().map(OsString::from).collect();
            args.push(exe.as_os_str().to_owned());
            (PathBuf::from(first), args)
        }
        None => (exe.to_path_buf(), Vec::new()),
    };
    let spec = Spec {
        program,
        args,
        env: vec![(var.into(), path)],
        cwd: exe.parent().unwrap().to_path_buf(),
        timeout: TIMEOUT,
        measure_rss: false,
    };
    let outcome = process::run(&spec).map_err(util::io_err(exe.display()))?;
    let mut out = outcome.stdout;
    if cfg!(windows) {
        // Standard output is a text stream there.
        out = String::from_utf8_lossy(&out)
            .replace("\r\n", "\n")
            .into_bytes();
    }
    if outcome.exit != Exit::Code(0) {
        out.extend_from_slice(format!("[{}]\n", outcome.exit.describe()).as_bytes());
        if !outcome.stderr.is_empty() {
            eprintln!(
                "{} wrote to stderr:\n{}",
                exe.display(),
                util::tail(&outcome.stderr, 20)
            );
        }
    }
    Ok(out)
}

/// The first line where `want` and `got` differ, with both versions.
fn first_difference(want: &[u8], got: &[u8]) -> String {
    let want = String::from_utf8_lossy(want);
    let got = String::from_utf8_lossy(got);
    let (w, g): (Vec<&str>, Vec<&str>) = (want.lines().collect(), got.lines().collect());
    let mut text = String::new();
    let mut shown = 0;
    for i in 0..w.len().max(g.len()) {
        let (a, b) = (w.get(i), g.get(i));
        if a != b {
            text.push_str(&format!(
                "line {}:\n  reference: {}\n  liborbit:  {}\n",
                i + 1,
                a.unwrap_or(&"(none)"),
                b.unwrap_or(&"(none)")
            ));
            shown += 1;
            if shown == 5 {
                text.push_str("  ...\n");
                break;
            }
        }
    }
    text
}

/// Every C API function the library exports is linked by one of `programs`
/// or listed in `tests/capi/not-linked.txt`; and every listed one is not.
fn linked_check(root: &Path, lib: &Library, programs: &[PathBuf]) -> Result<bool> {
    if !cfg!(target_os = "linux") {
        return Err("--check-linked runs on Linux only".into());
    }
    let exported = symbols(&["-D", "--defined-only"], &lib.file)?;
    let exported: BTreeSet<String> = exported.into_iter().filter(|s| is_c_api(s)).collect();
    // A program's dynamic symbols: the functions it imports, and data it
    // links (`lua_ident`), which the linker copies into the program.
    let mut linked = BTreeSet::new();
    for p in programs {
        linked.extend(symbols(&["-D"], p)?);
    }
    let list_path = root.join("tests/capi/not-linked.txt");
    let text = std::fs::read_to_string(&list_path).map_err(util::io_err(list_path.display()))?;
    let mut listed = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (fields, reason) = util::manifest_fields(line);
        let Some(name) = fields.first() else { continue };
        if reason.trim().is_empty() {
            return Err(format!("{}: {name} has no reason", rel(&list_path)));
        }
        listed.insert(name.to_string());
    }
    let missing: Vec<_> = exported
        .iter()
        .filter(|s| !linked.contains(*s) && !listed.contains(*s))
        .collect();
    let stale: Vec<_> = listed.iter().filter(|s| linked.contains(*s)).collect();
    let unknown: Vec<_> = listed.iter().filter(|s| !exported.contains(*s)).collect();
    println!(
        "\n{} C API functions exported, {} linked by these programs, {} listed as not linked",
        exported.len(),
        exported.iter().filter(|s| linked.contains(*s)).count(),
        listed.len()
    );
    for s in &missing {
        println!("  not linked by any program: {s}");
    }
    for s in &stale {
        println!("  listed in not-linked.txt but linked: {s}");
    }
    for s in &unknown {
        println!("  listed in not-linked.txt but not exported: {s}");
    }
    Ok(missing.is_empty() && stale.is_empty() && unknown.is_empty())
}

fn is_c_api(name: &str) -> bool {
    ["lua_", "luaL_", "luaopen_"]
        .iter()
        .any(|p| name.starts_with(p))
}

/// The symbol names `nm FLAGS file` lists, without version suffixes.
fn symbols(flags: &[&str], file: &Path) -> Result<Vec<String>> {
    let out = Command::new("nm")
        .args(flags)
        .arg(file)
        .output()
        .map_err(util::io_err("running nm"))?;
    if !out.status.success() {
        return Err(format!(
            "nm {} failed: {}",
            file.display(),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().last())
        .map(|s| s.split('@').next().unwrap_or(s).to_string())
        .collect())
}
