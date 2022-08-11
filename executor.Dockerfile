FROM ubuntu:22.04

COPY ./target/release/ballista-executor .

CMD ballista-executor
