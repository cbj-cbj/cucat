# 好友联机（把对方的小猫请到自己桌面上）

这个版本的 KuroCat 支持两个人在线串门：**你的猫会出现在对方桌面上，对方的猫也会出现在你桌面上**，双方都能看到对方猫咪的按键高亮、鼠标左右键和鼠标移动（和原版的动画完全一致）。

只同步**键位（虚拟键码）和鼠标动作**，不传输任何输入的文字内容。

---

## 1. 两种联机方式

| 方式 | 适用场景 | 需要什么 |
| --- | --- | --- |
| 🏠 局域网建房 | 同一 WiFi / 同一间屋子 | 什么都不用买，一方点「局域网建房」 |
| 🌐 中继服务器 | 异地联机（家里宽带 / 手机热点 / 公司网络） | 一台有公网 IP 的 VPS（几十块一年） |

两种方式用的是同一套协议，客户端代码完全一样；区别只是「谁在当中转站」。

---

## 2. 先在服务器上跑中继

中继是零依赖的单个 Node 脚本，对配置要求极低（1 核 512 MB 足够，2 个人只占用几 KB 带宽）。

```bash
# 1) 装 Node 18+（Ubuntu / Debian）
sudo apt update && sudo apt install -y nodejs

# 2) 把 relay/kurocat-relay.mjs 传到服务器
sudo mkdir -p /opt/kurocat-relay
sudo cp kurocat-relay.mjs /opt/kurocat-relay/

# 3) 直接试跑
node /opt/kurocat-relay/kurocat-relay.mjs --port 27422
# 看到 "KuroCat relay listening on 0.0.0.0:27422" 就成功了
```

长期运行建议用 systemd：

```bash
sudo useradd -r -s /usr/sbin/nologin kurocat || true
sudo cp kurocat-relay.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now kurocat-relay
sudo systemctl status kurocat-relay
```

放行端口（云厂商的「安全组」里也要放行同样的 TCP 端口）：

```bash
sudo ufw allow 27422/tcp
```

> ⚠️ 协议目前是**明文 TCP**。它只承载键位与鼠标坐标，不含文字；如果介意，可以在服务器上用 `stunnel` / `nginx stream` 套一层 TLS，或者干脆用 Tailscale 组网后走局域网模式。

验证服务器是否可达（在你自己电脑上）：

```powershell
Test-NetConnection relay.example.com -Port 27422
```

---

## 3. 在两个客户端上操作

1. 双方都右键桌面上的小猫 → **🐾 好友联机** 标签页。
2. 填 **你的显示名**（会显示在对方猫头上）。
3. **房间码**：两个人填**一模一样**的码，比如 `KURO`（字母数字，不区分大小写，最多 12 位）。
4. 公网联机：**中继服务器** 填 `你的服务器IP:27422`。
5. 两人都点 **🌐 加入房间**。

先加入的人会看到「等待好友加入…」，第二个人进来后，双方的猫会**自动出现在对方屏幕的左上/左侧**（贴着你自己猫的旁边），并显示对方的昵称小标签。

### 局域网（不买服务器）

1. A 点 **🏠 局域网建房**（默认端口 27422，可改）。
2. Windows 弹出防火墙提示时选 **允许访问**（专用网络即可）。
3. A 的状态栏会显示「房间已开启，端口 27422」，A 需要告诉 B 自己的内网 IP，例如 `192.168.1.23:27422`
   （在 PowerShell 里跑 `ipconfig` 看「IPv4 地址」）。
4. B 在 **中继服务器** 栏填 `192.168.1.23:27422`，房间码填一样的，点 **加入房间**。

> 家里宽带没有公网 IP（绝大多数情况）时，异地联机请用中继服务器，或者两人都装 Tailscale/ZeroTier，用它们的虚拟 IP 走局域网模式。

---

## 4. 看见的是什么 / 看不见什么

同步的内容：

- 键盘：按下的键（虚拟键码），和原版一样只显示键帽高亮与按键气泡，**不显示具体字符**
- 鼠标：左键 / 右键 / 中键按下、滚轮、鼠标在屏幕上的归一化位置（猫爪会跟着动）
- 外观：对方的猫咪皮肤（含 MCP 自定义猫）、2D/3D 模式、65%/108 键、正反视角、缩放、按键气泡开关、尾巴拍键模式、项圈颜色
- 隐私：对方在输入密码时，键位不会外传，你这边会看到猫咪闭眼捂键盘的防偷窥状态

不同步的内容：

- 输入的文字内容（协议里根本没有这个字段）
- 对方是否在玩游戏 / 耳机状态（避免误判）
- 摸头呼噜声（对方的猫在你这边是静音的）

---

## 5. 排错

| 现象 | 原因 / 解决 |
| --- | --- |
| 加入房间后一直「等待好友加入…」 | 房间码不一致；或对方还没点加入；或服务器端口没放行 |
| 提示「连接失败 / 超时」 | 服务器地址写错、端口被安全组挡住、服务器没启动 |
| 提示「房间已满（最多两人）」 | 这个房间码已经有两个人了，换一个 |
| 局域网建房后对方连不上 | 防火墙没允许、不在同一网段、路由器开了 AP 隔离 |
| 猫出现了但不动 | 对方在输入密码（防偷窥），或对方暂停了监听 |
| 想换位置 | 对方猫窗口是点击穿透的，位置跟随你自己的猫自动摆放；拖动自己的猫即可 |
| 完全看不到对方的猫 | Windows 7 兼容模式（ULW 分层窗口）不支持联机子窗口，请在 Win10/11 使用 |

关闭联机：菜单里点 **断开**，或直接退出程序（会自动断开房间并关闭本机中继）。

---

## 6. 协议速查（想自己写服务端时用）

传输层：TCP，UTF-8 换行分隔的 JSON（NDJSON）。

客户端 → 服务端：

```json
{"t":"join","room":"KURO","name":"小Q","v":1}
{"t":"msg","d":{"k":"input","ev":{"kind":"keyDown","vk":65,"scan":0,"repeat":false}}}
{"t":"msg","d":{"k":"state","s":{"name":"小Q","catId":"black","skin":null,"renderMode":"3d","keyboardLayout":"65","keyboardOrientation":"cat","scale":1,"showKeyBubbles":true,"tailKeyMode":"smart","tailKeyIds":["enter","space","bs","esc"],"collarColor":"none"}}}
{"t":"ping"}
{"t":"leave"}
```

服务端 → 客户端：

```json
{"t":"joined","room":"KURO","count":1,"you":"小Q"}
{"t":"peer","event":"join","name":"朋友","count":2}
{"t":"msg","d":{...},"from":"朋友"}
{"t":"pong"}
{"t":"error","msg":"房间已满（最多两人）"}
```

房间上限 2 人，服务端只做转发与进出房间通知，不解析 `d` 的内容。

---

## 7. 实现位置（改代码时看这里）

| 文件 | 作用 |
| --- | --- |
| `src-tauri/src/net.rs` | TCP 客户端 + 内建中继（局域网建房）、NDJSON 协议、心跳、房间管理 |
| `src-tauri/src/lib.rs` | `peer` 窗口的创建/摆放/显隐、`net_*` 命令、状态与外观广播 |
| `src/peerMain.ts` + `peer.html` | 朋友猫咪窗口的渲染（复用 2D/3D 引擎，和原版完全同一套动画） |
| `src/menuMain.ts` + `menu.html` | 菜单里的「🐾 好友联机」面板 |
| `relay/kurocat-relay.mjs` | 公网中继服务端（零依赖 Node） |
