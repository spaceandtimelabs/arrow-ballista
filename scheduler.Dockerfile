FROM ubuntu:22.04

RUN apt-get update && \
    apt-get install -y netcat

COPY ./target/release/ballista-scheduler .

EXPOSE 50050

CMD ./ballista-scheduler
