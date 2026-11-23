FROM golang:1.25-alpine AS builder
WORKDIR /src
COPY main.go .
RUN CGO_ENABLED=0 go build -o /api main.go
FROM alpine:3.20
COPY --from=builder /api /api
ENTRYPOINT ["/api"]
