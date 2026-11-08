# Pure-FTPd for the FTP tests (tools/test-env.sh): Debian's build has what servers in the wild have (FEAT, MLSD,
# MFMT, UTF8); Alpine's is cut down, the stilliard/pure-ftpd image is amd64 only and fails under Rosetta.
FROM debian:bookworm-slim
RUN apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends pure-ftpd \
    && rm -rf /var/lib/apt/lists/*
COPY start.sh /start.sh
CMD ["sh", "/start.sh"]
