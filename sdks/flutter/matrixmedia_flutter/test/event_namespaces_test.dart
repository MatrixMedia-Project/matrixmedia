import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:matrixmedia_flutter/matrixmedia_flutter.dart';

Map<String, dynamic> _block({
  Object? write = 'legacy',
  List<Object?>? read,
  Object? bot = '@mmbot:example.org',
  Object? version = 1,
}) => {
  'version': version,
  'write': write,
  'read':
      read ??
      [
        {'namespace': 'dev.matrixmedia', 'profile': 'v1'},
        {'namespace': 'com.matrixmedia', 'profile': 'v1'},
        {'namespace': 'com.steegler.matrixmedia', 'profile': 'v1'},
      ],
  'bot_user_id': bot,
};

Map<String, dynamic> _entry(String ns, [String profile = 'v1']) => {
  'namespace': ns,
  'profile': profile,
};

void main() {
  group('built-in list and matcher', () {
    final ns = MMEventNamespaces.builtIn;

    test('built-in namespaces, current namespace first', () {
      expect(kMMCurrentNamespace, 'dev.matrixmedia');
      expect(ns.readNamespaces, [
        'dev.matrixmedia',
        'com.matrixmedia',
        'com.steegler.matrixmedia',
      ]);
    });

    test('every legacy name matches its suffix', () {
      MMEventSuffix.legacyNames.forEach((suffix, legacy) {
        expect(ns.matches(legacy, suffix), isTrue, reason: legacy);
        expect(ns.match(legacy)!.builtIn, isTrue);
      });
    });

    test('every suffix matches under every built-in namespace', () {
      for (final suffix in MMEventSuffix.all) {
        for (final n in kMMBuiltInNamespaces) {
          expect(
            ns.matches('$n.$suffix', suffix),
            isTrue,
            reason: '$n.$suffix',
          );
        }
      }
    });

    test('the whole string must match; a prefix alone never does', () {
      expect(ns.match('dev.matrixmedia.'), isNull);
      expect(ns.match('dev.matrixmedia'), isNull);
      expect(ns.match('dev.matrixmedia.stream.extra'), isNull);
      expect(ns.match('dev.matrixmediax.stream'), isNull);
      expect(ns.match('xdev.matrixmedia.stream'), isNull);
      expect(ns.match('dev.matrixmedia.unknown_suffix'), isNull);
      expect(ns.match('com.matrixmedia.feed'), isNull);
      expect(ns.match('org.example.media.stream'), isNull);
      expect(ns.match('m.room.message'), isNull);
      expect(ns.match(''), isNull);
    });

    test('dotted suffixes resolve to the right suffix', () {
      expect(
        ns.match('com.matrixmedia.stream.e2ee_key')!.suffix,
        MMEventSuffix.streamE2eeKey,
      );
      expect(
        ns.match('dev.matrixmedia.feed.broadcast.started')!.suffix,
        MMEventSuffix.feedBroadcastStarted,
      );
      expect(ns.matches('dev.matrixmedia.stream.e2ee_key', 'stream'), isFalse);
    });

    test('typesFor lists the legacy name first, then the others', () {
      expect(ns.typesFor(MMEventSuffix.stream), [
        'com.matrixmedia.stream',
        'dev.matrixmedia.stream',
        'com.steegler.matrixmedia.stream',
      ]);
      expect(ns.typesFor(MMEventSuffix.roomConfig), [
        'dev.matrixmedia.room_config',
        'com.matrixmedia.room_config',
        'com.steegler.matrixmedia.room_config',
      ]);
    });
  });

  group('namespace validation (one failing case per rule)', () {
    test('accepts lower-case reverse-DNS', () {
      expect(isValidMMNamespace('org.example.media'), isTrue);
      expect(isValidMMNamespace('dev.matrixmedia'), isTrue);
      expect(isValidMMNamespace('a-b.c9'), isTrue);
      expect(isValidMMNamespace('a.${'b' * 62}'), isTrue); // 64 chars
    });

    test('rejects upper case', () {
      expect(isValidMMNamespace('Org.example'), isFalse);
    });
    test('rejects a single label', () {
      expect(isValidMMNamespace('matrixmedia'), isFalse);
    });
    test('rejects characters outside [a-z0-9-]', () {
      expect(isValidMMNamespace('org.exa_mple'), isFalse);
    });
    test('rejects empty labels', () {
      expect(isValidMMNamespace('org..example'), isFalse);
      expect(isValidMMNamespace('.org.example'), isFalse);
      expect(isValidMMNamespace('org.example.'), isFalse);
    });
    test('rejects more than 64 characters', () {
      expect(isValidMMNamespace('a.${'b' * 63}'), isFalse); // 65 chars
    });
    test('rejects m', () {
      expect(isValidMMNamespace('m'), isFalse);
    });
    test('rejects m.*', () {
      expect(isValidMMNamespace('m.room'), isFalse);
    });
    test('rejects org.matrix.*', () {
      expect(isValidMMNamespace('org.matrix.msc1234'), isFalse);
    });
    test('rejects im.vector.*', () {
      expect(isValidMMNamespace('im.vector.modular'), isFalse);
    });
    test('rejects io.element.*', () {
      expect(isValidMMNamespace('io.element.widgets'), isFalse);
    });
    test('rejects the empty string', () {
      expect(isValidMMNamespace(''), isFalse);
    });
  });

  group('announcement parsing', () {
    test('parses the documented block', () {
      final a = MMEventNamespaceAnnouncement.tryParse(_block())!;
      expect(a.version, 1);
      expect(a.writesLegacy, isTrue);
      expect(a.writeNamespace, isNull);
      expect(a.read.map((e) => e.namespace), kMMBuiltInNamespaces);
      expect(a.botUserId, '@mmbot:example.org');
    });

    test('missing, malformed and unknown-version blocks are ignored', () {
      expect(MMEventNamespaceAnnouncement.tryParse(null), isNull);
      expect(MMEventNamespaceAnnouncement.tryParse('legacy'), isNull);
      expect(MMEventNamespaceAnnouncement.tryParse(const []), isNull);
      expect(MMEventNamespaceAnnouncement.tryParse(const {}), isNull);
      expect(MMEventNamespaceAnnouncement.tryParse(_block(version: 2)), isNull);
      expect(
        MMEventNamespaceAnnouncement.tryParse(_block(version: '1')),
        isNull,
      );
    });

    test('a non-list read gives an empty read list, not an error', () {
      final a = MMEventNamespaceAnnouncement.tryParse({
        'version': 1,
        'write': 'dev.matrixmedia',
        'read': 'dev.matrixmedia',
      })!;
      expect(a.read, isEmpty);
      expect(a.writesLegacy, isTrue);
    });

    test('an unknown profile is dropped, the rest still apply', () {
      final a = MMEventNamespaceAnnouncement.tryParse(
        _block(
          read: [
            _entry('org.example.media', 'v2'),
            _entry('org.example.other'),
          ],
        ),
      )!;
      expect(a.read.map((e) => e.namespace), ['org.example.other']);
    });

    test('invalid entries are dropped silently', () {
      final a = MMEventNamespaceAnnouncement.tryParse(
        _block(
          read: [
            _entry('Org.Example'),
            _entry('m.room'),
            _entry('org.matrix.msc1'),
            'not-a-map',
            {'namespace': 7, 'profile': 'v1'},
            {'namespace': 'org.example.a'},
            _entry('org.example.ok'),
            _entry('org.example.ok'),
          ],
        ),
      )!;
      expect(a.read.map((e) => e.namespace), ['org.example.ok']);
    });

    test('at most eight entries are kept', () {
      final a = MMEventNamespaceAnnouncement.tryParse(
        _block(read: [for (var i = 0; i < 12; i++) _entry('org.example.n$i')]),
      )!;
      expect(a.read, hasLength(kMMMaxAnnouncedNamespaces));
      expect(a.read.last.namespace, 'org.example.n7');
    });

    test('write is honoured when it is a valid read entry', () {
      final a = MMEventNamespaceAnnouncement.tryParse(
        _block(
          write: 'org.example.media',
          read: [_entry('org.example.media'), _entry('dev.matrixmedia')],
        ),
      )!;
      expect(a.writeNamespace, 'org.example.media');
    });

    test('write falls back to legacy when not in read', () {
      final a = MMEventNamespaceAnnouncement.tryParse(
        _block(write: 'dev.matrixmedia', read: [_entry('com.matrixmedia')]),
      )!;
      expect(a.writesLegacy, isTrue);
    });

    test('write falls back to legacy when its read entry was invalid', () {
      final a = MMEventNamespaceAnnouncement.tryParse(
        _block(
          write: 'org.example.media',
          read: [_entry('org.example.media', 'v9')],
        ),
      )!;
      expect(a.writesLegacy, isTrue);
    });

    test('write falls back to legacy when missing or not a string', () {
      expect(
        MMEventNamespaceAnnouncement.tryParse(
          _block(write: null),
        )!.writesLegacy,
        isTrue,
      );
      expect(
        MMEventNamespaceAnnouncement.tryParse(_block(write: 3))!.writesLegacy,
        isTrue,
      );
    });

    test('a malformed bot_user_id is dropped', () {
      expect(
        MMEventNamespaceAnnouncement.tryParse(_block(bot: 'mmbot'))!.botUserId,
        isNull,
      );
      expect(
        MMEventNamespaceAnnouncement.tryParse(_block(bot: 5))!.botUserId,
        isNull,
      );
    });

    test('toJson round-trips', () {
      final a = MMEventNamespaceAnnouncement.tryParse(
        _block(write: 'dev.matrixmedia'),
      )!;
      final b = MMEventNamespaceAnnouncement.tryParse(
        jsonDecode(jsonEncode(a.toJson())),
      )!;
      expect(b.writeNamespace, 'dev.matrixmedia');
      expect(b.read, a.read);
      expect(b.botUserId, a.botUserId);
    });
  });

  group('write choice', () {
    test('no announcement writes the legacy names', () {
      expect(
        mmEventTypeToWrite(MMEventSuffix.feedPost),
        'com.steegler.matrixmedia.feed.post',
      );
      expect(
        mmEventTypeToWrite(MMEventSuffix.feedMuted),
        'com.steegler.matrixmedia.feed_muted',
      );
      expect(
        mmEventTypeToWrite(MMEventSuffix.reactionRecents),
        'com.steegler.matrixmedia.reaction_recents',
      );
    });

    test('"legacy" writes the legacy names', () {
      final a = MMEventNamespaceAnnouncement.tryParse(_block())!;
      expect(
        a.writeTypeFor(MMEventSuffix.feedPost),
        'com.steegler.matrixmedia.feed.post',
      );
    });

    test('a namespace writes <namespace>.<suffix>', () {
      final a = MMEventNamespaceAnnouncement.tryParse(
        _block(write: 'dev.matrixmedia'),
      )!;
      expect(
        a.writeTypeFor(MMEventSuffix.feedPost),
        'dev.matrixmedia.feed.post',
      );
      final b = MMEventNamespaceAnnouncement.tryParse(
        _block(write: 'org.example.media', read: [_entry('org.example.media')]),
      )!;
      expect(
        mmEventTypeToWrite(MMEventSuffix.feedPost, announcement: b),
        'org.example.media.feed.post',
      );
    });

    test('a suffix with no legacy name uses the current namespace', () {
      expect(
        mmEventTypeToWrite(MMEventSuffix.roomConfig),
        'dev.matrixmedia.room_config',
      );
    });
  });

  group('effective read list', () {
    MMEventNamespaceAnnouncement announce(
      List<String> extra, {
      String bot = '@mmbot:example.org',
    }) => MMEventNamespaceAnnouncement.tryParse(
      _block(
        bot: bot,
        read: [
          _entry('dev.matrixmedia'),
          _entry('com.matrixmedia'),
          _entry('com.steegler.matrixmedia'),
          for (final e in extra) _entry(e),
        ],
      ),
    )!;

    test('an announcement adds namespaces', () {
      final ns = MMEventNamespaces.builtIn.merge(
        announce(['org.example.media']),
      );
      expect(ns.readNamespaces, [...kMMBuiltInNamespaces, 'org.example.media']);
      expect(ns.matches('org.example.media.stream', 'stream'), isTrue);
      expect(ns.match('org.example.media.stream')!.builtIn, isFalse);
      expect(ns.match('org.example.media.unknown'), isNull);
    });

    test('built-in names cannot be removed', () {
      final a = MMEventNamespaceAnnouncement.tryParse(
        _block(read: [_entry('org.example.media')]),
      );
      final ns = MMEventNamespaces.builtIn.merge(a);
      for (final n in kMMBuiltInNamespaces) {
        expect(ns.readNamespaces, contains(n));
        expect(ns.matches('$n.stream', 'stream'), isTrue);
      }
    });

    test('a later, shorter announcement does not shrink the list', () {
      final ns = MMEventNamespaces.builtIn
          .merge(announce(['org.example.a', 'org.example.b']))
          .merge(announce(['org.example.c']))
          .merge(announce(const []));
      expect(ns.announcedExtras, [
        'org.example.a',
        'org.example.b',
        'org.example.c',
      ]);
    });

    test('a null announcement changes nothing', () {
      final ns = MMEventNamespaces.builtIn.merge(announce(['org.example.a']));
      expect(identical(ns.merge(null), ns), isTrue);
    });

    test('the union keeps at most eight announced entries', () {
      var ns = MMEventNamespaces.builtIn;
      ns = ns.merge(announce(['org.example.a', 'org.example.b']));
      ns = ns.merge(
        announce([
          'org.example.c',
          'org.example.d',
          'org.example.e',
          'org.example.f',
        ]),
      );
      // 3 built-in + 6 extra announced = 9 > 8: the last one is not kept.
      expect(ns.announcedExtras, hasLength(5));
      expect(ns.announcedExtras, isNot(contains('org.example.f')));
    });

    test('bots of every announcement are kept', () {
      final ns = MMEventNamespaces.builtIn
          .merge(announce(['org.example.a'], bot: '@one:example.org'))
          .merge(announce(['org.example.a'], bot: '@two:example.org'));
      expect(ns.botsFor('org.example.a'), {
        '@one:example.org',
        '@two:example.org',
      });
      expect(ns.botsFor('dev.matrixmedia'), isEmpty);
    });

    test('persists and restores', () {
      final ns = MMEventNamespaces.builtIn.merge(announce(['org.example.a']));
      final restored = MMEventNamespaces.fromJson(
        jsonDecode(jsonEncode(ns.toJson())),
      );
      expect(restored.readNamespaces, ns.readNamespaces);
      expect(restored.botsFor('org.example.a'), {'@mmbot:example.org'});
    });

    test('a malformed stored value restores to the built-in list', () {
      for (final bad in [
        null,
        'x',
        7,
        const {},
        {'version': 2},
      ]) {
        expect(
          MMEventNamespaces.fromJson(bad).readNamespaces,
          kMMBuiltInNamespaces,
        );
      }
      final partial = MMEventNamespaces.fromJson({
        'version': 1,
        'announced': ['m.room', 'org.example.ok', 5],
        'extras': [
          {'namespace': 'm.room', 'profile': 'v1', 'bots': []},
          {
            'namespace': 'org.example.ok',
            'profile': 'v1',
            'bots': ['bad', '@b:x'],
          },
        ],
      });
      expect(partial.announcedExtras, ['org.example.ok']);
      expect(partial.botsFor('org.example.ok'), {'@b:x'});
    });
  });

  group('sender rule', () {
    final ns = MMEventNamespaces.builtIn.merge(
      MMEventNamespaceAnnouncement.tryParse(
        _block(read: [_entry('org.example.media')]),
      ),
    );

    test('built-in namespaces need no particular sender', () {
      expect(
        ns.honouredMatch('com.matrixmedia.stream', senderId: '@anyone:x'),
        isNotNull,
      );
      expect(
        ns.honouredMatch('dev.matrixmedia.donation', senderId: null),
        isNotNull,
      );
    });

    test('server-written suffixes under an announced name need the bot', () {
      for (final suffix in MMEventSuffix.serverWritten) {
        final type = 'org.example.media.$suffix';
        expect(
          ns.honouredMatch(type, senderId: '@mmbot:example.org'),
          isNotNull,
          reason: type,
        );
        expect(ns.honouredMatch(type, senderId: '@mallory:x'), isNull);
        expect(ns.honouredMatch(type, senderId: null), isNull);
      }
    });

    test('client-written suffixes under an announced name take any sender', () {
      expect(
        ns.honouredMatch('org.example.media.feed.post', senderId: '@alice:x'),
        isNotNull,
      );
    });

    test('an announced name without a bot honours no server-written event', () {
      final noBot = MMEventNamespaces.builtIn.merge(
        MMEventNamespaceAnnouncement.tryParse(
          _block(bot: null, read: [_entry('org.example.media')]),
        ),
      );
      expect(
        noBot.honouredMatch('org.example.media.stream', senderId: '@mmbot:x'),
        isNull,
      );
    });
  });

  test('X-MM-Client header format', () {
    expect(
      formatMMClientHeader(
        clientId: 'matrixmedia-fluffychat',
        version: '2.4.0',
        platform: 'android',
        build: '3546',
      ),
      'matrixmedia-fluffychat/2.4.0 (android; build 3546)',
    );
    expect(
      formatMMClientHeader(
        clientId: 'matrixmedia-fluffychat',
        version: '1.0 beta(x)',
        platform: '',
        build: '1;2',
      ),
      'matrixmedia-fluffychat/1.0betax (unknown; build 12)',
    );
  });
}
