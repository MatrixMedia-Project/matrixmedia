# MatrixMedia Distribution

## Docker Image

Pre-built Docker image for mm-core:

```bash
# Load from file
docker load < mm-core-0.1.0-docker.tar.gz

# Verify
docker run --rm matrixmedia/mm-core:latest --help

# Run
docker run -d \
  -p 6167:6167 \
  -p 6168:6168 \
  -p 9090:9090 \
  -e MM_MATRIX_HOMESERVER_URL=http://synapse:8008 \
  -e MM_MATRIX_SERVER_NAME=example.org \
  -e MM_MATRIX_AS_TOKEN=your-as-token \
  -e MM_MATRIX_HS_TOKEN=your-hs-token \
  -e MM_JWT_SIGNING_KEY=your-32-byte-signing-key \
  -e MM_ADMIN_TOKEN=your-admin-token \
  -e MM_SFU_LIVEKIT_URL=http://livekit:7880 \
  -e MM_SFU_LIVEKIT_API_KEY=devkey \
  -e MM_SFU_LIVEKIT_API_SECRET=devsecret \
  matrixmedia/mm-core:0.1.0
```

## Image Details

- **Size:** 39 MB compressed / 130 MB uncompressed
- **Base:** debian:bookworm-slim
- **User:** matrixmedia (non-root)
- **Ports:** 6167 (client API), 6168 (admin API), 9090 (Prometheus metrics)
- **Entrypoint:** `matrixmedia serve`

## Publishing to GHCR

```bash
# Tag for GitHub Container Registry
docker load < mm-core-0.1.0-docker.tar.gz
docker tag matrixmedia/mm-core:0.1.0 ghcr.io/YOUR_ORG/mm-core:0.1.0
docker tag matrixmedia/mm-core:0.1.0 ghcr.io/YOUR_ORG/mm-core:latest

# Login and push
echo $GITHUB_TOKEN | docker login ghcr.io -u YOUR_USERNAME --password-stdin
docker push ghcr.io/YOUR_ORG/mm-core:0.1.0
docker push ghcr.io/YOUR_ORG/mm-core:latest
```
