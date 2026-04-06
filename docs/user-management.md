# Synapse User Management

Admin user: `@te1:localhost`

All commands use the Synapse Admin API. First get a token:

```bash
HS="http://localhost:8008"
TOKEN=$(curl -s -X POST "$HS/_matrix/client/v3/login" \
  -H "Content-Type: application/json" \
  -d '{"type":"m.login.password","user":"te1","password":"YOUR_PASSWORD"}' \
  | grep -o '"access_token":"[^"]*"' | cut -d'"' -f4)
```

## List Users

```bash
curl -s "$HS/_synapse/admin/v2/users?limit=50" \
  -H "Authorization: Bearer $TOKEN" | python3 -m json.tool
```

## Create User

```bash
curl -s -X PUT "$HS/_synapse/admin/v2/users/@newuser:localhost" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "password": "securepassword123",
    "displayname": "New User",
    "admin": false
  }'
```

## Reset Password

```bash
curl -s -X PUT "$HS/_synapse/admin/v2/users/@someuser:localhost" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"password": "newpassword123"}'
```

## Make User Admin

```bash
curl -s -X PUT "$HS/_synapse/admin/v2/users/@someuser:localhost" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"admin": true}'
```

## Deactivate User

```bash
curl -s -X POST "$HS/_synapse/admin/v1/deactivate/@someuser:localhost" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"erase": false}'
```

## Get User Info

```bash
curl -s "$HS/_synapse/admin/v2/users/@someuser:localhost" \
  -H "Authorization: Bearer $TOKEN" | python3 -m json.tool
```

## List User's Rooms

```bash
curl -s "$HS/_synapse/admin/v1/users/@someuser:localhost/joined_rooms" \
  -H "Authorization: Bearer $TOKEN" | python3 -m json.tool
```

## List User's Devices

```bash
curl -s "$HS/_synapse/admin/v2/users/@someuser:localhost/devices" \
  -H "Authorization: Bearer $TOKEN" | python3 -m json.tool
```

## Delete User's Device (force logout)

```bash
curl -s -X DELETE "$HS/_synapse/admin/v2/users/@someuser:localhost/devices/DEVICE_ID" \
  -H "Authorization: Bearer $TOKEN"
```

## List All Rooms

```bash
curl -s "$HS/_synapse/admin/v1/rooms?limit=50" \
  -H "Authorization: Bearer $TOKEN" | python3 -m json.tool
```

## Delete Room

```bash
curl -s -X DELETE "$HS/_synapse/admin/v2/rooms/ROOM_ID" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"purge": true}'
```

## Cleanup: Delete Test Users

```bash
# Delete all tester* users (from test client sessions)
for USER in $(curl -s "$HS/_synapse/admin/v2/users?limit=100" \
  -H "Authorization: Bearer $TOKEN" | python3 -c "
import sys, json
for u in json.load(sys.stdin).get('users',[]):
    if 'tester' in u['name']:
        print(u['name'])
"); do
  echo "Deactivating $USER..."
  curl -s -X POST "$HS/_synapse/admin/v1/deactivate/$USER" \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"erase": true}' > /dev/null
done
```
