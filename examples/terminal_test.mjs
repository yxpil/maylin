// 终端 WebSocket 测试：连接 demo-node 终端，发送 stdin 行，验证输出回显
const token = process.env.MAYLIN_TOKEN;
const ws = new WebSocket(`ws://127.0.0.1:7000/api/instances/demo-node/terminal?token=${token}`);
let got = 0;
ws.onopen = () => {
  console.log("[terminal] connected");
  ws.send("console.log('ECHO-TEST-' + (123+456))");
};
ws.onmessage = (ev) => {
  try {
    const l = JSON.parse(ev.data);
    if (l.text.includes("[stdin] echo:")) {
      console.log("[terminal] OK: stdin 输入已在实例 stdout 回显 ->", l.text.trim());
      got = 1;
      ws.close();
      process.exit(0);
    }
  } catch {}
};
ws.onerror = (e) => { console.log("[terminal] error", e.message); process.exit(1); };
setTimeout(() => { if (!got) { console.log("[terminal] 超时未收到回显"); process.exit(1); } }, 10000);
