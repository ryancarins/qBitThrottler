FROM rust:1.80-alpine3.19

COPY ./ ./

RUN cargo build --release

CMD ["./target/release/qBitThrottler"]