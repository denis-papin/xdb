# Reproducible build + benchmark run of the xdb POC.
#   docker build -t xdb-poc .
#   docker run --rm -v $PWD/bench:/out xdb-poc --out /out        (add --quick for a short run)

# ---- builder: compile and run the whole test suite (std only, no external crates)
FROM rust:1-slim AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY workloads ./workloads
RUN cargo build --release -p xdb-bench
RUN cargo test --release

# ---- runtime: just the binary (the workloads are embedded in it)
# rust:1-slim tracks Debian stable, so the same glibc is in debian:stable-slim.
FROM debian:stable-slim
COPY --from=builder /src/target/release/xdb-bench /usr/local/bin/xdb-bench
WORKDIR /out
ENTRYPOINT ["/usr/local/bin/xdb-bench"]
CMD ["--out", "/out"]
