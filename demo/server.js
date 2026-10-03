// Maylin 演示实例：Node.js HTTP 服务
// 用法: node server.js --port 3001
// 支持 stdin：向终端输入的行会被回显到 stdout（演示 WebSocket 终端）
const http = require('http');

const args = process.argv.slice(2);
const idx = args.indexOf('--port');
const port = (idx >= 0 && parseInt(args[idx + 1])) || parseInt(process.env.PORT) || 3000;

process.stdin.setEncoding('utf8');
let stdinBuf = '';
process.stdin.on('data', (d) => {
  stdinBuf += d;
  let i;
  while ((i = stdinBuf.indexOf('\n')) >= 0) {
    const line = stdinBuf.slice(0, i).trim();
    stdinBuf = stdinBuf.slice(i + 1);
    if (line) console.log(`[stdin] echo: ${line}`);
  }
});

const server = http.createServer((req, res) => {
  if (req.url.startsWith('/health')) {
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ ok: true, pid: process.pid, port }));
  } else {
    res.writeHead(200, { 'Content-Type': 'text/plain; charset=utf-8' });
    res.end(`hello from node-demo (pid=${process.pid}, port=${port})\n`);
  }
});

server.listen(port, () => {
  console.log(`[node-demo] listening on ${port}, pid=${process.pid}`);
});
