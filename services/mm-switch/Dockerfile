FROM golang:1.24-alpine AS builder
WORKDIR /app
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 go build -o mm-switch .

FROM alpine:3.21
RUN apk add --no-cache ca-certificates
COPY --from=builder /app/mm-switch /usr/local/bin/mm-switch
EXPOSE 7890
ENTRYPOINT ["mm-switch"]
