# 该 Dockerfile 接受 GitHub Actions 预编译的静态 musl 二进制 + 已构建的前端产物。
# 不在镜像里跑 cargo / pnpm，构建超快、镜像超小。
#
# 构建时通过 buildx --platform 自动注入 TARGETARCH（amd64 / arm64），
# 对应 artifacts/kiro-rs-linux-{amd64,arm64} 这两个二进制文件。

FROM alpine:3.21

RUN apk add --no-cache ca-certificates

WORKDIR /app

ARG TARGETARCH
COPY artifacts/kiro-rs-linux-${TARGETARCH} /app/kiro-rs
COPY admin-ui/dist /app/admin-ui/dist

RUN chmod +x /app/kiro-rs

VOLUME ["/app/config"]

EXPOSE 8990

CMD ["./kiro-rs", "-c", "/app/config/config.json", "--credentials", "/app/config/credentials.json"]
