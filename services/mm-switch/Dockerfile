FROM golang:1.24-alpine AS builder
ENV GOTOOLCHAIN=auto
WORKDIR /app
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 go build -o mm-switch .

FROM alpine:3.21
# ffmpeg is used at recording finalise to extract a poster frame for
# the dashboard / mobile VOD lists (see record_webm.go::Finalise).
RUN apk add --no-cache ca-certificates ffmpeg
COPY --from=builder /app/mm-switch /usr/local/bin/mm-switch
EXPOSE 7890
ENTRYPOINT ["mm-switch"]
