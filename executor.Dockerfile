FROM ubuntu:22.04

WORKDIR /root

COPY ./target/release/ballista-executor /root

COPY ./ballista/rust/client/testdata/delta-table /root

EXPOSE 50051

CMD ./ballista-executor --scheduler-host=$SCHEDULER_HOST
