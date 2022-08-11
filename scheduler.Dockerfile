FROM ubuntu:22.04

COPY ./target/release/ballista-scheduler .

CMD ballista-scheduler
