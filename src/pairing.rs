//! The details the user copies into the iPhone shortcut.

use std::net::{IpAddr, UdpSocket};
use std::path::Path;

use crate::config::Config;

pub fn info_text(config: &Config, config_path: &Path) -> String {
    let port = config.port;
    let address = match local_ip() {
        Some(ip) => format!("http://{ip}:{port}"),
        None => format!("（没检测到局域网地址）:{port}"),
    };
    let host = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "本机名".into());
    format!(
        "在 iPhone 快捷指令里填写：\n\n\
         地址：{address}\n\
         密钥：{token}\n\n\
         如果电脑的局域网地址会变，也可以试试用电脑名：\n\
         http://{host}.local:{port}\n\n\
         小技巧：用 iPhone 相机对准这个窗口，能直接用「实况文本」拷贝上面的文字。\n\n\
         配置文件：{path}",
        token = config.token,
        path = config_path.display(),
    )
}

/// The address of the interface that holds the default route. Connecting a
/// UDP socket only selects a route; nothing is sent.
fn local_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    socket.connect(("192.0.2.1", 9)).ok()?; // TEST-NET-1, never actually contacted
    socket.local_addr().ok().map(|addr| addr.ip()).filter(|ip| !ip.is_unspecified())
}
