import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
import 'package:matrixmedia_flutter/src/mm_api_client.dart';

void main() {
  MMApiClient apiWith(void Function(http.Request) onRequest) {
    final client = MockClient((request) async {
      onRequest(request);
      return http.Response('{}', 200);
    });
    return MMApiClient(baseUrl: 'https://mm.example', httpClient: client)
      ..setToken('mm-token', 'refresh-token');
  }

  group('fleet viewer count', () {
    test(
      'uses the authenticated fleet proxy, not the direct switch list',
      () async {
        final requests = <http.Request>[];
        final client = MockClient((request) async {
          requests.add(request);
          return http.Response('{"stream_id":"abc","viewers":3}', 200);
        });
        final api = MMApiClient(
          baseUrl: 'https://mm.example',
          httpClient: client,
        )..setToken('mm-token', 'refresh-token');

        expect(await api.getStreamViewerCount('abc'), 3);
        expect(requests, hasLength(1));
        expect(
          requests.single.url.toString(),
          'https://mm.example/_mm/fleet/v1/streams/abc/api/viewers',
        );
        expect(requests.single.headers['Authorization'], 'Bearer mm-token');
      },
    );

    test('refresh keeps the fleet prefix when the session expires', () async {
      final paths = <String>[];
      var viewerCalls = 0;
      final client = MockClient((request) async {
        paths.add(request.url.path);
        if (request.url.path.endsWith('/auth/refresh')) {
          return http.Response(
            '{"mm_token":"fresh-mm","refresh_token":"fresh-refresh","user_id":"@a:example","expires_in":900}',
            200,
          );
        }
        viewerCalls++;
        if (viewerCalls == 1) {
          return http.Response(
            '{"error":"MM_TOKEN_EXPIRED","message":"invalid token: ExpiredSignature"}',
            403,
          );
        }
        return http.Response('{"stream_id":"abc","viewers":4}', 200);
      });
      final api = MMApiClient(baseUrl: 'https://mm.example', httpClient: client)
        ..setToken('expired-mm', 'refresh-token');

      expect(await api.getStreamViewerCount('abc'), 4);
      expect(paths, [
        '/_mm/fleet/v1/streams/abc/api/viewers',
        '/_mm/client/v1/auth/refresh',
        '/_mm/fleet/v1/streams/abc/api/viewers',
      ]);
    });
  });

  group('stream captions opt-in', () {
    test('getStreamCaptions hits the per-stream endpoint', () async {
      http.Request? seen;
      final api = apiWith((r) => seen = r);
      await api.getStreamCaptions('abc');
      expect(
        seen!.url.toString(),
        'https://mm.example/_mm/client/v1/streams/abc/captions',
      );
      expect(seen!.method, 'GET');
    });

    test('setStreamCaptions PUTs the opt-in choice', () async {
      http.Request? seen;
      final api = apiWith((r) => seen = r);
      await api.setStreamCaptions('abc', 'on');
      expect(
        seen!.url.toString(),
        'https://mm.example/_mm/client/v1/streams/abc/captions',
      );
      expect(seen!.method, 'PUT');
      expect(seen!.body, '{"opt_in":"on"}');
    });

    test('getCreatorCaptions hits the creator default endpoint', () async {
      http.Request? seen;
      final api = apiWith((r) => seen = r);
      await api.getCreatorCaptions();
      expect(
        seen!.url.toString(),
        'https://mm.example/_mm/client/v1/creator/me/captions',
      );
      expect(seen!.method, 'GET');
    });
  });

  group('stream share grants', () {
    MMApiClient apiReturning(String body) {
      final client = MockClient((_) async => http.Response(body, 200));
      return MMApiClient(baseUrl: 'https://mm.example', httpClient: client)
        ..setToken('mm-token', 'refresh-token');
    }

    test('createStreamShareGrant POSTs link_access public and returns the grant', () async {
      http.Request? seen;
      final api = apiWith((r) => seen = r);
      // The mock answers {} so the shape assertion lives in the
      // integration path; here we pin method, path, and body.
      await api.createStreamShareGrant('abc', linkAccess: 'public');
      expect(
        seen!.url.toString(),
        'https://mm.example/_mm/client/v1/streams/abc/share-grants',
      );
      expect(seen!.method, 'POST');
      expect(seen!.body, '{"link_access":"public"}');
    });

    test('createStreamShareGrant decodes the minted grant', () async {
      final api = apiReturning(
        '{"grant_id":"g1","stream_id":"abc","expires_at":"2026-11-13T04:33:07Z","link_access":"public","paywall_price_sats":null}',
      );
      final grant = await api.createStreamShareGrant('abc', linkAccess: 'public');
      expect(grant['grant_id'], 'g1');
      expect(grant['link_access'], 'public');
    });

    test('revokeStreamShareGrant DELETEs the grant', () async {
      http.Request? seen;
      final api = apiWith((r) => seen = r);
      await api.revokeStreamShareGrant('abc', 'g1');
      expect(
        seen!.url.toString(),
        'https://mm.example/_mm/client/v1/streams/abc/share-grants/g1',
      );
      expect(seen!.method, 'DELETE');
    });

    test('shareWatchUrl builds the anonymous viewer link on the API origin', () async {
      final api = apiWith((_) {});
      expect(
        api.shareWatchUrl('abc', 'g1'),
        'https://mm.example/_mm/viewer/watch/abc?grant=g1',
      );
    });
  });
}
