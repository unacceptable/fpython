//! `fpython bundle`: assemble a minimal root filesystem for a `FROM scratch` image when a
//! program uses the CPython fallback (dynamic libpython + stdlib + site-packages + shared libs).
//! Fully native programs don't need this: build them with `--static` instead.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Shared libraries an ELF file depends on (resolved paths), via `ldd`.
pub fn ldd(file: &Path) -> Vec<PathBuf> {
    let out = match Command::new("ldd").arg(file).output() {
        Ok(o) => o,
        Err(_) => return vec![],
    };
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    parse_ldd(&text)
}

/// Parse `ldd` output (glibc and musl formats).
pub fn parse_ldd(text: &str) -> Vec<PathBuf> {
    let mut v = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        // "libfoo.so => /lib/libfoo.so (0x...)"  or  "/lib/ld-musl-x86_64.so.1 (0x...)"
        let path = if let Some((_, rhs)) = line.split_once("=>") {
            rhs.trim().split_whitespace().next().unwrap_or("").to_string()
        } else {
            line.split_whitespace().next().unwrap_or("").to_string()
        };
        if path.starts_with('/') {
            v.push(PathBuf::from(path));
        }
    }
    v
}

fn copy_into(rootfs: &Path, src: &Path, copied: &mut BTreeSet<PathBuf>) -> std::io::Result<()> {
    if !copied.insert(src.to_path_buf()) {
        return Ok(());
    }
    let rel = src.strip_prefix("/").unwrap_or(src);
    let dst = rootfs.join(rel);
    if let Some(p) = dst.parent() {
        std::fs::create_dir_all(p)?;
    }
    // keep symlinks' targets too (libraries are often symlinked)
    if let Ok(target) = std::fs::read_link(src) {
        let resolved = if target.is_absolute() { target } else { src.parent().unwrap().join(target) };
        copy_into(rootfs, &resolved, copied)?;
    }
    std::fs::copy(src, &dst)?;
    Ok(())
}

fn copy_tree(rootfs: &Path, dir: &Path, copied: &mut BTreeSet<PathBuf>, so_files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        // trim parts of the stdlib a server never needs
        if matches!(name.as_str(), "test" | "idlelib" | "tkinter" | "turtledemo" | "ensurepip") {
            continue;
        }
        let ft = e.file_type()?;
        if ft.is_dir() {
            copy_tree(rootfs, &p, copied, so_files)?;
        } else if ft.is_file() || ft.is_symlink() {
            if name.ends_with(".so") || name.contains(".so.") {
                so_files.push(p.clone());
            }
            copy_into(rootfs, &p, copied)?;
        }
    }
    Ok(())
}

pub struct BundleReport {
    pub files: usize,
    pub libs: usize,
}

/// Build `rootfs` containing `exe` (at `/<name>`), its libraries and the Python home.
pub fn bundle(exe: &Path, rootfs: &Path, python: &str) -> Result<BundleReport, String> {
    std::fs::create_dir_all(rootfs).map_err(|e| e.to_string())?;
    let mut copied = BTreeSet::new();
    let name = exe.file_name().ok_or("bad executable path")?;
    std::fs::copy(exe, rootfs.join(name)).map_err(|e| format!("copy {}: {}", exe.display(), e))?;
    let mut libs: BTreeSet<PathBuf> = ldd(exe).into_iter().collect();
    // Python home (stdlib + lib-dynload + site-packages)
    let q = |code: &str| -> Result<String, String> {
        let o = Command::new(python).args(["-c", code]).output().map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let stdlib = q("import sysconfig; print(sysconfig.get_path('stdlib'))")?;
    let site = q("import sysconfig; print(sysconfig.get_path('purelib'))")?;
    let mut so_files = Vec::new();
    for d in [stdlib.clone(), site.clone()] {
        if !d.is_empty() && Path::new(&d).is_dir() && !copied.contains(Path::new(&d)) {
            copy_tree(rootfs, Path::new(&d), &mut copied, &mut so_files).map_err(|e| format!("{}: {}", d, e))?;
        }
    }
    for so in &so_files {
        libs.extend(ldd(so));
    }
    let nlibs = libs.len();
    for l in libs {
        copy_into(rootfs, &l, &mut copied).map_err(|e| format!("{}: {}", l.display(), e))?;
    }
    // TLS roots and a writable /tmp
    for certs in ["/etc/ssl/certs/ca-certificates.crt", "/etc/ssl/cert.pem"] {
        if Path::new(certs).exists() {
            let _ = copy_into(rootfs, Path::new(certs), &mut copied);
        }
    }
    let _ = std::fs::create_dir_all(rootfs.join("tmp"));
    Ok(BundleReport { files: copied.len() + 1, libs: nlibs })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_glibc_and_musl_ldd() {
        let glibc = "\tlinux-vdso.so.1 (0x00007ffd)\n\tlibpython3.14.so.1.0 => /usr/local/lib/libpython3.14.so.1.0 (0x00007f)\n\tlibc.so.6 => /lib/x86_64-linux-gnu/libc.so.6 (0x00007f)\n\t/lib64/ld-linux-x86-64.so.2 (0x00007f)\n";
        let v = parse_ldd(glibc);
        assert_eq!(v, vec![PathBuf::from("/usr/local/lib/libpython3.14.so.1.0"), PathBuf::from("/lib/x86_64-linux-gnu/libc.so.6"), PathBuf::from("/lib64/ld-linux-x86-64.so.2")]);
        let musl = "\t/lib/ld-musl-aarch64.so.1 (0xffff)\n\tlibpython3.14.so.1.0 => /usr/local/lib/libpython3.14.so.1.0 (0xffff)\n\tlibc.musl-aarch64.so.1 => /lib/ld-musl-aarch64.so.1 (0xffff)\n";
        let v = parse_ldd(musl);
        assert!(v.contains(&PathBuf::from("/lib/ld-musl-aarch64.so.1")));
        assert!(v.contains(&PathBuf::from("/usr/local/lib/libpython3.14.so.1.0")));
        assert!(parse_ldd("not a dynamic executable").is_empty());
    }

    #[test]
    fn bundle_copies_executable_and_python_home() {
        let d = std::env::temp_dir().join(format!("fpy-bundle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let exe = std::env::current_exe().unwrap();
        // use a tiny fake "python" via env: point stdlib/purelib at an empty temp dir
        let home = d.join("home");
        std::fs::create_dir_all(home.join("pkg")).unwrap();
        std::fs::write(home.join("pkg").join("m.py"), "x = 1\n").unwrap();
        // skipped stdlib parts, extension modules, and relative symlinks
        std::fs::create_dir_all(home.join("test")).unwrap();
        std::fs::write(home.join("test").join("big.py"), "").unwrap();
        std::fs::write(home.join("pkg").join("_ext.cpython-314.so"), "").unwrap();
        std::fs::write(home.join("pkg").join("libx.so.1.0"), "").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("libx.so.1.0", home.join("pkg").join("libx.so")).unwrap();
        let fake = d.join("fakepy.sh");
        std::fs::write(&fake, format!("#!/bin/sh\necho {}\n", home.display())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let r = bundle(&exe, &d.join("rootfs"), fake.to_str().unwrap()).unwrap();
        assert!(r.files >= 2);
        assert!(d.join("rootfs").join(exe.file_name().unwrap()).exists());
        let rel = home.strip_prefix("/").unwrap();
        assert!(d.join("rootfs").join(rel).join("pkg/m.py").exists());
        assert!(d.join("rootfs").join(rel).join("pkg/libx.so").exists());
        assert!(!d.join("rootfs").join(rel).join("test").exists());
        assert!(d.join("rootfs/tmp").is_dir());
        // errors: a missing executable, a python that can't run
        assert!(bundle(&d.join("missing"), &d.join("r2"), fake.to_str().unwrap()).is_err());
        assert!(bundle(&exe, &d.join("r3"), "/nonexistent/python").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
