FROM rust:bookworm
COPY entrypoint.sh /entrypoint.sh
RUN chmod a+x /entrypoint.sh

RUN rustup default stable && rustup update

ENTRYPOINT ["/entrypoint.sh"]
