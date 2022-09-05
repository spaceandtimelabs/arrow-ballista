FROM ubuntu:22.04

WORKDIR /root

COPY ./target/release/ballista-scheduler .

COPY ./ballista/rust/client/testdata/delta-table /root

EXPOSE 50050

CMD ./ballista-scheduler
