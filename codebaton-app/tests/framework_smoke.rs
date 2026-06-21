//! 测试框架骨架冒烟测试：仅验证 harness/fixture/断言 helper 能编译并运行，
//! 不测产品行为（具体 CLI-SS 用例待 QA 出 docs/test-design-automation.md 后再补）。

mod common;

use common::*;

/// 冒烟：构建 TwoBackend（A 发送 + B 接收守护）、做一次最小推送、用断言 helper
/// 验证，最后 run_root drop 时 RAII 回收磁盘。此测试存在的意义是让框架骨架进入
/// 编译与运行链路；产品级断言由后续用例承担。
#[test]
fn framework_skeleton_builds_and_tears_down() {
    let run_root_path;
    {
        let h = TwoBackend::builder()
            .a_file("README.md", "# smoke\n")
            .a_file("src/main.rs", "fn main() {}\n")
            .build();
        run_root_path = h.run_root.path().to_path_buf();
        assert!(run_root_path.exists(), "run_root 应在 harness 存活期间存在");

        // 最小推送 + 断言 helper 链路。
        let report = h.push(false).expect("空目录推送应成功");
        assert!(report.code_files_transferred >= 2);
        assert_dir_tree_eq(h.a_dir(), h.b_dir()).unwrap();
        assert_no_backup_in(h.b_dir()).unwrap();
        assert_no_trash_in(h.b_dir()).unwrap();
        assert_snapshot_synced(h.a_snapshot()).unwrap();
    }
    // h 已 drop → run_root TempDir 递归删除。
    assert!(
        !run_root_path.exists(),
        "RAII：harness drop 后 run_root 应被回收"
    );
}
