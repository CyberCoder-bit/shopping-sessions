# Shopping Sessions: Rust pipeline.
# The scorers are linux/amd64 (Intel Xeon, Cascade Lake or newer) and build this file
# natively. Build has network (<= 10 min); runs have none, so everything is compiled here.

FROM rust:1.97.1-slim-bookworm AS build
WORKDIR /src
COPY .cargo .cargo
COPY Cargo.toml Cargo.lock ./
# Compile dependencies in their own layer so source edits rebuild fast.
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked \
 && rm -rf src
COPY src src
RUN touch src/main.rs && cargo build --release --locked && strip target/release/pipeline

FROM debian:bookworm-slim
COPY --from=build /src/target/release/pipeline /app/pipeline
# At run time: uid 1000, read-only root (only /tmp, 64 MB, in memory), no network,
# /data read-only, /output and /scratch writable. HOME/TMPDIR point at /scratch.
ENV HOME=/scratch TMPDIR=/scratch
# Keep the many 0.5-3 MB partition buffers in malloc arenas (no per-buffer mmap/munmap,
# whose mmap-lock writes stall other threads' page faults) and never trim them.
# Measured on the 20M bench: median wall 1.47 s -> 1.37 s (6 of 7 interleaved pairs).
ENV GLIBC_TUNABLES=glibc.malloc.mmap_threshold=33554432:glibc.malloc.trim_threshold=4294967295
USER 1000:1000
WORKDIR /app
CMD ["/app/pipeline"]
