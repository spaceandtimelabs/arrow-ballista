FROM ubuntu:22.04

COPY ./target/release/ballista-executor .

EXPOSE 50051

CMD ./ballista-executor
