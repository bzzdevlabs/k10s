#!/bin/sh
# Entrypoint-style batch script on busybox: templating, text tools, archives, a background job stopped by a trap.
set -eu
mkdir -p /srv/www /var/log/app
cat > /srv/www/index.html <<HTML
<h1>${HOSTNAME:-unknown}</h1>
HTML
sed -i 's/h1/h2/g' /srv/www/index.html
for i in $(seq 1 50); do echo "line $i $(date +%s)" >> /var/log/app/log; done
sort -r /var/log/app/log | head -n 3 | wc -l
grep -c line /var/log/app/log
nproc
awk '/MemTotal/ {print $2}' /proc/meminfo
id
ls -la /srv/www | tail -n +2
find /etc -name '*.conf' | head -n 5
tar -cf /tmp/www.tar -C /srv www && gzip -9 /tmp/www.tar && ls -l /tmp/www.tar.gz
md5sum /srv/www/index.html
sha256sum /bin/busybox | cut -c1-16
ps
uptime
df -h / | tail -n 1
(while true; do date >> /var/log/app/heartbeat; sleep 0.1; done) &
child=$!
trap 'kill $child; wait $child || true; echo stopped' EXIT
until [ "$(wc -l < /var/log/app/heartbeat 2>/dev/null || echo 0)" -ge 5 ]; do sleep 0.1; done
echo done
