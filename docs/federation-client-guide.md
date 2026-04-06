# Federation Client Developer Guide

Guide for building MM-aware clients that work across federation.

## Discovering MM Servers

When a user is in a Matrix room with an active MM stream, the state event contains
everything needed:

```typescript
const streamEvent = room.getStateEvent("com.matrixmedia.stream", "");
const mmServerUrl = streamEvent.content.mm_server_url;
const mmMatrixServer = streamEvent.content.mm_matrix_server;
```

## Authenticating with a Foreign MM Server

```typescript
// 1. Get OpenID token from YOUR homeserver
const openidResp = await matrixClient.getOpenIdToken();
// { access_token, token_type, matrix_server_name, expires_in }

// 2. Exchange with foreign MM server
const mmAuthResp = await fetch(`${mmServerUrl}/_mm/client/v1/auth/token`, {
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({ openid_token: openidResp })
});
const { mm_token } = await mmAuthResp.json();

// 3. Use mm_token for all subsequent MM API calls
```

## Handling Federation Rejections

```typescript
try {
  await authenticateWithMM(mmServerUrl, openidToken);
} catch (err) {
  if (err.status === 403 && err.code === "MM_FORBIDDEN") {
    // Federation denied by MM server
    // Possibilities:
    //   - Your home server is in their deny_list
    //   - Their allow_list doesn't include your server
    //   - Federation disabled entirely
    showMessage(`Cannot join this stream from your server.`);
  }
}
```

## Verifying MM Server Identity

Before trusting a federated MM server, verify it via `.well-known`:

```typescript
async function verifyMMServer(mmServerUrl: string, expectedMatrixServer: string) {
  const resp = await fetch(`${mmServerUrl}/.well-known/matrix/matrixmedia`);
  const info = await resp.json();
  
  if (info.mm_server.matrix_server !== expectedMatrixServer) {
    throw new Error("MM server does not match expected Matrix server");
  }
  
  return info.mm_server;
}
```

## Capability Detection

Clients should adapt to the foreign MM server's capabilities:

```typescript
const info = await verifyMMServer(mmServerUrl, mmMatrixServer);

if (info.e2ee.required && !clientSupportsE2EE) {
  showMessage("This stream requires E2EE. Update your client.");
  return;
}

if (info.recording_enabled) {
  showRecordingIndicator();
}
```

## Checklist

- [ ] Read mm_server_url from stream state event
- [ ] Get OpenID token from user's home server
- [ ] Exchange with foreign MM server's /auth/token
- [ ] Handle 403 Forbidden (federation denied)
- [ ] Verify foreign server via .well-known before sensitive operations
- [ ] Respect e2ee.required flag
- [ ] Cache MM JWT until expiry (don't re-authenticate per request)
