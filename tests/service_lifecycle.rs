//! 在实际 Windows 管理员终端手动运行：cargo test --test service_lifecycle -- --ignored
#![cfg(windows)]
use rastflow::search::{helper::HelperSession, win::service};
use std::process::Command;
use std::time::Duration;

struct InstalledService;
impl Drop for InstalledService {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_rastflow"))
            .arg("--uninstall-service")
            .status();
    }
}

#[test]
#[ignore = "需要 Windows 管理员权限，会安装并卸载索引服务；不得已有服务"]
fn service_install_start_reject_foreign_client_and_uninstall() {
    assert_eq!(
        service::state(),
        service::ServiceState::NotInstalled,
        "先移除已有服务，测试不能覆盖用户安装"
    );
    let executable = env!("CARGO_BIN_EXE_rastflow");
    let status = Command::new(executable)
        .arg("--install-service")
        .status()
        .unwrap();
    let _cleanup = InstalledService;
    assert!(
        status.success(),
        "安装失败，请确认管理员权限并查看 service.log"
    );
    assert!(service::wait_until_running(Duration::from_secs(15)));
    assert!(service::process_id().is_some());
    // 测试 runner 不是安装位置的桌面 exe，服务必须拒绝它。
    assert!(HelperSession::connect_running_service().is_err());
    assert!(
        Command::new(executable)
            .arg("--uninstall-service")
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(service::state(), service::ServiceState::NotInstalled);
}
