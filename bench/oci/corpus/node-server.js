// HTTP server reading a file per request (libuv thread pool), stopping on SIGTERM like a container app should.
const http = require('http');
const fs = require('fs');
const crypto = require('crypto');
const server = http.createServer((req, res) => {
  fs.readFile('/etc/hostname', (err, data) => {
    const h = crypto.createHash('sha256').update(data || '').digest('hex');
    res.end(`hello ${h.slice(0, 8)}\n`);
  });
});
server.listen(3000);
process.on('SIGTERM', () => server.close(() => process.exit(0)));
