#!/bin/sh
# Test servers for the network sources in Docker: MinIO (S3), OpenSSH (SFTP), WebDAV twice — rclone and
# Apache mod_dav — and FTP twice — vsftpd and pure-ftpd (each pair answers the same requests differently).
#
#   tools/test-env.sh             start (create if missing), print the exports for the tests
#   tools/test-env.sh down        stop the containers
#   tools/test-env.sh latency MS  delay through the proxy: localhost:2223 is the sshd with MS ms each way
#                                 (round trip 2×MS); 0 removes the delay
#
# Use: eval "$(tools/test-env.sh)" && UISON_REQUIRE_NET=1 cargo test --workspace
# Keys and known_hosts live in target/test-env/ (generated once; a new key pair recreates the sshd container),
# the FTP servers' self-signed certificate in target/test-env/ftp/.
set -eu

cd "$(dirname "$0")/.."
DIR="$PWD/target/test-env"
MINIO=uison-minio
SSHD=uison-sshd
PROXY=uison-toxiproxy
DAV=uison-webdav
DAV2=uison-webdav-apache
FTP=uison-ftp
FTP2=uison-ftp-pure
PASS=test-password
PHRASE=test-passphrase

if [ "${1:-}" = down ]; then
    docker stop "$MINIO" "$SSHD" "$PROXY" "$DAV" "$DAV2" "$FTP" "$FTP2" >/dev/null 2>&1 || true
    echo "stopped $MINIO $SSHD $PROXY $DAV $DAV2 $FTP $FTP2" >&2
    exit 0
fi

if [ "${1:-}" = latency ]; then
    ms=${2:?latency in ms}
    api=http://localhost:8474/proxies/sftp/toxics
    curl -s -X DELETE "$api/down" >/dev/null; curl -s -X DELETE "$api/up" >/dev/null
    if [ "$ms" != 0 ]; then
        curl -s -X POST "$api" -d "{\"name\":\"down\",\"type\":\"latency\",\"stream\":\"downstream\",\"attributes\":{\"latency\":$ms}}" >/dev/null
        curl -s -X POST "$api" -d "{\"name\":\"up\",\"type\":\"latency\",\"stream\":\"upstream\",\"attributes\":{\"latency\":$ms}}" >/dev/null
    fi
    echo "localhost:2223 → sshd with ${ms} ms each way" >&2
    exit 0
fi

command -v docker >/dev/null || { echo "docker is not installed" >&2; exit 1; }
if ! docker info >/dev/null 2>&1; then
    command -v orb >/dev/null && orb start >/dev/null 2>&1 || true
    docker info >/dev/null 2>&1 || { echo "the docker daemon is not running" >&2; exit 1; }
fi

mkdir -p "$DIR"
fresh=0
if [ ! -f "$DIR/id_plain" ] || [ ! -f "$DIR/id_pass" ]; then
    rm -f "$DIR"/id_plain* "$DIR"/id_pass*
    ssh-keygen -q -t ed25519 -N "" -C uison-test -f "$DIR/id_plain"
    ssh-keygen -q -t ed25519 -N "$PHRASE" -C uison-test -f "$DIR/id_pass"
    fresh=1
fi

exists() { docker container inspect "$1" >/dev/null 2>&1; }
running() { [ "$(docker container inspect -f '{{.State.Running}}' "$1" 2>/dev/null)" = true ]; }

# MinIO: bitnamilegacy/minio (minio/minio would not start on this machine).
if ! exists "$MINIO"; then
    docker run -d --name "$MINIO" -p 9000:9000 -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
        bitnamilegacy/minio:latest >/dev/null
elif ! running "$MINIO"; then
    docker start "$MINIO" >/dev/null
fi

# sshd: the public keys are baked in at creation, so new keys mean a new container.
if exists "$SSHD" && [ "$fresh" = 1 ]; then
    docker rm -f "$SSHD" >/dev/null
fi
if ! exists "$SSHD"; then
    docker run -d --name "$SSHD" -p 2222:2222 -e PUID=1000 -e PGID=1000 -e USER_NAME=uison \
        -e USER_PASSWORD="$PASS" -e PASSWORD_ACCESS=true \
        -e PUBLIC_KEY="$(cat "$DIR/id_plain.pub")
$(cat "$DIR/id_pass.pub")" \
        lscr.io/linuxserver/openssh-server:latest >/dev/null
elif ! running "$SSHD"; then
    docker start "$SSHD" >/dev/null
fi

# A proxy with adjustable delay in front of sshd (a far server): localhost:2223.
if ! exists "$PROXY"; then
    docker run -d --name "$PROXY" -p 8474:8474 -p 2223:2223 ghcr.io/shopify/toxiproxy:2.9.0 >/dev/null
elif ! running "$PROXY"; then
    docker start "$PROXY" >/dev/null
fi

# WebDAV: rclone on 8081, Apache on 8082; the same user and password.
if ! exists "$DAV"; then
    docker run -d --name "$DAV" -p 8081:80 rclone/rclone:1.68 serve webdav /data --addr :80 --user uison --pass "$PASS" >/dev/null
elif ! running "$DAV"; then
    docker start "$DAV" >/dev/null
fi
if ! exists "$DAV2"; then
    docker run -d --name "$DAV2" -p 8082:80 -e AUTH_TYPE=Basic -e USERNAME=uison -e PASSWORD="$PASS" bytemark/webdav >/dev/null
elif ! running "$DAV2"; then
    docker start "$DAV2" >/dev/null
fi

# FTP: one self-signed certificate for both servers (FTPS asks about it, as it would about any such server).
mkdir -p "$DIR/ftp"
if [ ! -f "$DIR/ftp/cert.pem" ]; then
    openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj /CN=localhost -addext subjectAltName=DNS:localhost,IP:127.0.0.1 \
        -keyout "$DIR/ftp/key.pem" -out "$DIR/ftp/cert.pem" 2>/dev/null
fi
# vsftpd: LIST only, MDTM sets times, the home is /ftp/uison, data connections must resume the TLS session.
# Port 2121 — plain FTP and explicit TLS; 2990 — implicit TLS (a second vsftpd).
if ! exists "$FTP"; then
    docker run -d --name "$FTP" -p 2121:21 -p 2990:990 -p 21000-21019:21000-21019 -v "$DIR/ftp:/certs:ro" \
        -e USERS="uison|$PASS" delfer/alpine-ftp-server sh -c '
/bin/start_vsftpd.sh true
TLS="-opasv_address=127.0.0.1 -orsa_cert_file=/certs/cert.pem -orsa_private_key_file=/certs/key.pem -ossl_enable=YES -oallow_anon_ssl=NO -ossl_tlsv1=NO -ossl_sslv2=NO -ossl_sslv3=NO -ossl_ciphers=HIGH"
vsftpd -opasv_min_port=21000 -opasv_max_port=21009 $TLS -oforce_local_data_ssl=NO -oforce_local_logins_ssl=NO /etc/vsftpd/vsftpd.conf
vsftpd -opasv_min_port=21010 -opasv_max_port=21019 $TLS -oimplicit_ssl=YES -olisten_port=990 /etc/vsftpd/vsftpd.conf
sleep infinity' >/dev/null
elif ! running "$FTP"; then
    docker start "$FTP" >/dev/null
fi
# pure-ftpd (Debian's build: tools/pure-ftpd/): MLSD, MFMT, the home is /. Port 2131 — plain FTP and explicit TLS.
if ! exists "$FTP2"; then
    docker image inspect uison-pure-ftpd >/dev/null 2>&1 || docker build -q -t uison-pure-ftpd tools/pure-ftpd >/dev/null
    docker run -d --name "$FTP2" --cap-add SYS_NICE --cap-add DAC_READ_SEARCH -p 2131:21 -p 30000-30009:30000-30009 \
        -e FTP_PASSWORD="$PASS" -v "$DIR/ftp:/certs:ro" uison-pure-ftpd >/dev/null
elif ! running "$FTP2"; then
    docker start "$FTP2" >/dev/null
fi

i=0
until [ "$(curl -s -o /dev/null -w '%{http_code}' http://localhost:9000/minio/health/live)" = 200 ]; do
    i=$((i + 1)); [ $i -gt 30 ] && { echo "MinIO did not come up" >&2; exit 1; }; sleep 1
done
i=0
until ssh-keyscan -p 2222 localhost 2>/dev/null | grep -q ed25519; do
    i=$((i + 1)); [ $i -gt 60 ] && { echo "sshd did not come up" >&2; exit 1; }; sleep 1
done
i=0
until curl -s http://localhost:8474/version >/dev/null; do
    i=$((i + 1)); [ $i -gt 30 ] && { echo "toxiproxy did not come up" >&2; exit 1; }; sleep 1
done
for port in 8081 8082; do
    i=0
    until [ "$(curl -s -o /dev/null -w '%{http_code}' -u "uison:$PASS" -X PROPFIND -H 'Depth: 0' http://localhost:$port/)" = 207 ]; do
        i=$((i + 1)); [ $i -gt 30 ] && { echo "WebDAV on $port did not come up" >&2; exit 1; }; sleep 1
    done
done
for port in 2121 2990 2131; do
    i=0
    # OrbStack accepts a connection to a published port before the server listens: wait for its greeting.
    until [ "$(curl -s -k --max-time 3 -o /dev/null -w '%{http_code}' "$([ $port = 2990 ] && echo ftps || echo ftp)://localhost:$port/" \
        -u "uison:$PASS" 2>/dev/null)" != 000 ]; do
        i=$((i + 1)); [ $i -gt 30 ] && { echo "FTP on $port did not come up" >&2; exit 1; }; sleep 1
    done
done
# The proxy reaches sshd through the host (both publish their ports there).
curl -s -X POST http://localhost:8474/proxies \
    -d '{"name":"sftp","listen":"0.0.0.0:2223","upstream":"host.docker.internal:2222"}' >/dev/null || true
# A fresh container has a new host key: forget the old one.
[ "$fresh" = 1 ] && rm -f "$DIR/known_hosts"

cat <<EOF
export UISON_S3_ENDPOINT=http://localhost:9000 UISON_S3_KEY=minioadmin UISON_S3_SECRET=minioadmin
export UISON_SFTP_SERVER=uison@localhost:2222 UISON_SFTP_PASSWORD=$PASS
export UISON_SFTP_KEY=$DIR/id_plain UISON_SFTP_KEY_PASS=$DIR/id_pass UISON_SFTP_PASSPHRASE=$PHRASE
export UISON_KNOWN_HOSTS=$DIR/known_hosts
export UISON_FTP_SERVERS="ftp://localhost:2121 ftpes://localhost:2121 ftps://localhost:2990 ftp://localhost:2131 ftpes://localhost:2131"
export UISON_FTP_USER=uison UISON_FTP_PASSWORD=$PASS
export UISON_WEBDAV_URL=http://localhost:8081/ UISON_WEBDAV_URL2=http://localhost:8082/ UISON_WEBDAV_USER=uison UISON_WEBDAV_PASSWORD=$PASS
# export UISON_SFTP_SERVER=uison@localhost:2223   # through the proxy: tools/test-env.sh latency 25
EOF
