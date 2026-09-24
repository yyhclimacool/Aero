//! Explicit opt-in acceptance: downloads only into this workspace's target directory.
use aero_embedding::{DIMENSION, DOWNLOAD_BYTES, LocalEmbedder, download_model, model_available};
use std::{cell::Cell, path::PathBuf, sync::atomic::AtomicBool, time::Instant};

#[test]
#[ignore = "requires the pinned 135 MB public model; set AERO_EMBEDDING_TEST_ROOT under target"]
fn download_and_rank_real_model() {
    let root = PathBuf::from(
        std::env::var_os("AERO_EMBEDDING_TEST_ROOT").expect("explicit target directory required"),
    );
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target").canonicalize().unwrap();
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    assert!(root.starts_with(target), "never test with a real user cache");
    let start = Instant::now();
    let progress = Cell::new(0);
    download_model(&root, &AtomicBool::new(false), |n, total| {
        assert_eq!(total, DOWNLOAD_BYTES);
        assert!(n >= progress.get() && n <= total);
        progress.set(n);
    })
    .unwrap();
    eprintln!(
        "model bytes={} download={:.3}s root={}",
        progress.get(),
        start.elapsed().as_secs_f64(),
        root.display()
    );
    assert_eq!(progress.get(), DOWNLOAD_BYTES);
    assert!(model_available(&root));
    let start = Instant::now();
    let mut model = LocalEmbedder::load(&root).unwrap();
    eprintln!("local load={:.3}s", start.elapsed().as_secs_f64());
    let passages: Vec<String> = [
        "给猫咪准备干净的饮水和猫粮，定期接种疫苗，陪它玩耍。",
        "忘记登录密码时，可以通过绑定的邮箱重置密码，恢复账户访问。",
        "乘坐高速铁路去旅行，提前购买车票，记得带身份证进站。",
        "定期把电脑中的重要文件复制到移动硬盘，避免数据意外丢失。",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let queries: Vec<String> =
        ["怎样照顾喵星人？", "忘了口令如何找回账号？", "坐高铁出游要准备什么？", "怎样备份电脑资料？"]
            .into_iter()
            .map(str::to_owned)
            .collect();
    let start = Instant::now();
    let documents = model.embed(&passages, false).unwrap();
    let vectors = model.embed(&queries, true).unwrap();
    for (i, q) in vectors.iter().enumerate() {
        let mut ranks: Vec<_> = documents
            .iter()
            .enumerate()
            .map(|(j, p)| (j, p.iter().zip(q).map(|(a, b)| a * b).sum::<f32>()))
            .collect();
        ranks.sort_by(|a, b| b.1.total_cmp(&a.1));
        eprintln!("{} => {:?}", queries[i], ranks);
        assert_eq!(ranks[0].0, i);
    }
    for v in documents.iter().chain(&vectors) {
        assert_eq!(v.len(), DIMENSION);
        assert!(v.iter().all(|x| x.is_finite()));
        assert!((v.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-5);
    }
    eprintln!("8 texts (two batches of four) inference={:.3}s", start.elapsed().as_secs_f64());
    assert!(model.embed(&vec!["large batch".into(); 5], false).is_err());
    assert!(model.embed(&[], true).unwrap().is_empty());
    let long = model.embed(&["测试长句子截断而不产生溢出窗口。".repeat(500)], false).unwrap();
    assert_eq!(long[0].len(), DIMENSION);
    // Keep model loading as well as inference within the opt-in memory budget.
    #[cfg(target_os = "macos")]
    {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        // SAFETY: getrusage initializes the pointed-to structure on success.
        assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) }, 0);
        let peak = unsafe { usage.assume_init() }.ru_maxrss;
        eprintln!("peak RSS={peak} bytes");
        assert!(peak < 640 * 1024 * 1024, "embedding worker exceeded 640 MiB peak RSS");
    }
    // Drop owns/reclaims the session; the process-global ORT environment may remain.
    drop(model);
}
