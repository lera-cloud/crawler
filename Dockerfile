FROM --platform=$BUILDPLATFORM rust:bullseye AS builder

WORKDIR /app
COPY . .

ENV GRPC_HOST=0.0.0.0:50055
ENV GRPC_HOST_API=api:50051

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
    gcc cmake libc6 libssl-dev

RUN cargo install --no-default-features --path .

FROM --platform=$BUILDPLATFORM debian:bullseye-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential \
    ca-certificates libssl-dev

COPY --from=builder /usr/local/cargo/bin/website_crawler /usr/local/bin/website_crawler
COPY --from=builder /usr/local/cargo/bin/health_client /usr/local/bin/health_client

ENV GRPC_HOST=0.0.0.0:50055
ENV GRPC_HOST_API=api:50051

CMD ["website_crawler"]