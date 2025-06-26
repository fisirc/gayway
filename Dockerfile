FROM rust:alpine3.22 AS builder

WORKDIR /app

RUN apk add --no-cache \
    musl-dev \
    pkgconfig \
    perl \
    make \
    clang-dev \
    libstdc++ \
    musl

COPY . .

RUN --mount=type=cache,target=/app/target \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --target x86_64-unknown-linux-musl --release && \
    mv target/x86_64-unknown-linux-musl/release/nur_gateway /app/nur_gateway

FROM alpine:3.22

WORKDIR /

COPY --from=builder /app/nur_gateway /nur_gateway

CMD ["/nur_gateway"]
