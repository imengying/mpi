//! `pi update`: replace the running binary with the latest GitHub Release build.
//!
//! The flow mirrors `install.sh` — query the API for the latest tag, pick the asset for
//! the running target, verify GitHub's own sha256 digest of the download, then swap the
//! binary in place. Hashing and archive extraction shell out to the system `sha256sum` /
//! `shasum` and `tar`: the agent already prefers system tools, and a hashing crate for
//! one command a release-cycle does not earn its place in the dependency list.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};

use crate::cli;

const REPO: &str = "imengying/mpi";

/// Entry point from `main`: drives the async flow on a throwaway runtime.
pub fn run() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("构建 Tokio 运行时失败")?
        .block_on(update())
}

async fn update() -> Result<()> {
    let triple = target_triple().context(
        "当前平台没有对应的 Release 产物（只有 Linux / macOS 的 x86_64 / aarch64）",
    )?;
    let current = cli::version();

    let http = reqwest::Client::builder()
        // GitHub's API rejects requests without a User-Agent with 403, and reqwest sends
        // none by default.
        .user_agent(format!("pi/{}", cli::version()))
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(300))
        .build()?;
    let release: serde_json::Value = http
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("请求 {REPO} 的 Release 信息失败"))?
        .error_for_status()
        .context("获取最新 Release 失败（API 匿名限流 60 次/小时）")?
        .json()
        .await?;

    let latest = release["tag_name"]
        .as_str()
        .context("Release 信息里没有 tag_name")?
        .trim_start_matches('v')
        .to_string();
    if latest == current {
        println!("已是最新版本：pi {latest}");
        return Ok(());
    }
    // A source build, or a build from a tag that has no Release yet, can report something
    // newer than the latest Release. Replacing it would quietly move the user backwards.
    if newer_than(current, &latest) {
        println!("本地 pi {current} 比最新 Release {latest} 更新，保持不变。");
        println!("要强制安装 Release 版，请运行 install.sh。");
        return Ok(());
    }

    // A release always carries all four targets, so a miss here means the asset naming
    // changed — say so instead of downloading something guessed at.
    let asset_name = format!("pi-{latest}-{triple}.tar.gz");
    let asset = release["assets"]
        .as_array()
        .context("Release 信息里没有资产列表")?
        .iter()
        .find(|asset| asset["name"].as_str() == Some(asset_name.as_str()))
        .with_context(|| format!("Release {latest} 里没有 {asset_name}"))?;
    let digest = asset["digest"]
        .as_str()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .context("GitHub 未提供该资产的 sha256，拒绝在未校验的情况下替换自身")?
        .to_lowercase();
    let url = asset["browser_download_url"]
        .as_str()
        .context("Release 信息里没有下载地址")?
        .to_string();

    println!("==> pi {current} → {latest}（{triple}）");
    let tarball = http
        .get(url)
        .send()
        .await
        .context("下载失败")?
        .error_for_status()
        .context("下载失败")?
        .bytes()
        .await?;

    let work = std::env::temp_dir().join(format!("pi-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let archive = work.join(&asset_name);
    std::fs::write(&archive, &tarball)?;

    let actual = sha256_hex(&archive)?;
    ensure!(
        actual == digest,
        "sha256 不匹配：期望 {digest}，实际 {actual}"
    );
    println!("==> sha256 校验通过");

    extract_binary(&archive, &work)?;
    let downloaded = work.join("pi");
    let reported = std::process::Command::new(&downloaded)
        .arg("--version")
        .output()
        .context("无法运行下载的二进制")?;
    ensure!(
        reported.status.success()
            && String::from_utf8_lossy(&reported.stdout).trim() == format!("pi {latest}"),
        "下载的二进制没有报告 pi {latest}"
    );

    replace_current_exe(&downloaded, &current_exe()?)?;
    println!("==> 已更新到 pi {latest}");
    let _ = std::fs::remove_dir_all(&work);
    Ok(())
}

/// Is `a` a numerically newer version than `b`?
///
/// Only plain dotted numbers are compared. Anything else — a `dev` build string, a commit
/// hash, a `-rc1` suffix — counts as unknown and is never treated as newer, because
/// refusing an update on a guess would be worse than allowing it.
fn newer_than(a: &str, b: &str) -> bool {
    fn parse(version: &str) -> Option<Vec<u64>> {
        let parts: Option<Vec<u64>> = version.split('.').map(|part| part.parse().ok()).collect();
        parts.filter(|parts| !parts.is_empty())
    }
    match (parse(a), parse(b)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// The release target this build corresponds to, or `None` where nothing is published.
pub fn target_triple() -> Option<&'static str> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => Some("x86_64-unknown-linux-gnu"),
        ("aarch64", "linux") => Some("aarch64-unknown-linux-gnu"),
        ("x86_64", "macos") => Some("x86_64-apple-darwin"),
        ("aarch64", "macos") => Some("aarch64-apple-darwin"),
        _ => None,
    }
}

/// Hash with the system hasher — `sha256sum` on Linux, `shasum` on macOS.
fn sha256_hex(path: &Path) -> Result<String> {
    let output = std::process::Command::new("sha256sum")
        .arg(path)
        .output()
        .or_else(|_| {
            std::process::Command::new("shasum")
                .arg("-a")
                .arg("256")
                .arg(path)
                .output()
        })
        .context("找不到 sha256sum / shasum，无法校验下载")?;
    ensure!(output.status.success(), "计算 sha256 失败");
    Ok(String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase())
}

/// Unwrap `pi-<版本>-<target>/pi` out of the release archive with the system tar.
fn extract_binary(archive: &Path, dir: &Path) -> Result<()> {
    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(dir)
        .arg("--strip-components=1")
        .status()
        .context("找不到 tar")?;
    ensure!(status.success(), "解压 {archive:?} 失败");
    ensure!(dir.join("pi").is_file(), "压缩包里没有 pi 二进制");
    Ok(())
}

fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().context("定位当前二进制失败")
}

/// Swap the binary under the running process: stage next to it (same filesystem, so the
/// rename is atomic) and rename over. `fs::copy` carries the permission bits over.
fn replace_current_exe(new_binary: &Path, exe: &Path) -> Result<()> {
    let staged = exe.with_file_name(".pi.new");
    std::fs::copy(new_binary, &staged)
        .with_context(|| format!("写入 {} 失败（目录可写吗？）", staged.display()))?;
    std::fs::rename(staged, exe).with_context(|| format!("替换 {} 失败", exe.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotted_versions_compare_numerically() {
        // Not lexicographic: 0.9 < 0.10, and 9 answers to 2.
        assert!(newer_than("0.1.10", "0.1.9"));
        assert!(newer_than("0.2", "0.1.99"));
        assert!(newer_than("1.0", "0.9.9"));
        assert!(!newer_than("0.1.9", "0.1.10"));
        assert!(!newer_than("0.1.3", "0.1.3"));
    }

    #[test]
    fn unparsable_versions_never_count_as_newer() {
        // A dev build tells the user to force the install instead of blocking on a guess.
        for (a, b) in [
            ("dev", "0.1.3"),
            ("0.1.3-rc1", "0.1.3"),
            ("", "0.1.3"),
            ("0.1.x", "0.1.3"),
        ] {
            assert!(!newer_than(a, b), "{a} vs {b}");
        }
    }

    #[test]
    fn the_release_target_is_one_we_publish() {
        // `None` is legitimate on other platforms; what must not happen is a triple that no
        // release carries, which would send `update` looking for an asset that cannot exist.
        if let Some(triple) = target_triple() {
            assert!(
                [
                    "x86_64-unknown-linux-gnu",
                    "aarch64-unknown-linux-gnu",
                    "x86_64-apple-darwin",
                    "aarch64-apple-darwin",
                ]
                .contains(&triple),
                "意外的 target：{triple}"
            );
        }
    }
}
