#!/bin/sh
# User uison / $FTP_PASSWORD in /home/uison; TLS with /certs/{key,cert}.pem (plain FTP allowed too); passive
# ports 30000-30009 announced as 127.0.0.1.
set -e
mkdir -p /etc/ssl/private /home/uison
cat /certs/key.pem /certs/cert.pem > /etc/ssl/private/pure-ftpd.pem
chmod 600 /etc/ssl/private/pure-ftpd.pem
id uison >/dev/null 2>&1 || useradd -d /home/uison uison
chown uison /home/uison
rm -f /etc/pureftpd.passwd
printf '%s\n%s\n' "$FTP_PASSWORD" "$FTP_PASSWORD" | pure-pw useradd uison -u uison -d /home/uison -f /etc/pureftpd.passwd >/dev/null
pure-pw mkdb /etc/pureftpd.pdb -f /etc/pureftpd.passwd
exec pure-ftpd -l puredb:/etc/pureftpd.pdb -E -j -P 127.0.0.1 -p 30000:30009 -Y 1 -c 20 -C 20
