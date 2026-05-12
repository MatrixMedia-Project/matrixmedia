FROM golang:1.24-alpine AS builder
ENV GOTOOLCHAIN=auto
WORKDIR /app
# go.mod has `replace github.com/at-wat/ebml-go => ./internal-ebml-fork`
# (Rec.Fix.3 — TrackEntry field-order patch). The fork dir must be
# present BEFORE `go mod download` resolves the replace target;
# otherwise download fails with "directory does not exist".
COPY go.mod go.sum ./
COPY internal-ebml-fork/ ./internal-ebml-fork/
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 go build -o mm-switch .

FROM alpine:3.21
# ffmpeg is used at recording finalise to extract a poster frame for
# the dashboard / mobile VOD lists (see record_webm.go::Finalise).
RUN apk add --no-cache ca-certificates ffmpeg && \
    addgroup -S mmswitch && adduser -S -G mmswitch mmswitch
COPY --from=builder /app/mm-switch /usr/local/bin/mm-switch
USER mmswitch
EXPOSE 7890
ENTRYPOINT ["mm-switch"]
