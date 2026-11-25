import 'dart:async';
import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:http/http.dart' as http;
import 'package:http/testing.dart';
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
    test('rejects the bare reserved names (brief 7.1)', () {
      expect(isValidMMNamespace('org.matrix'), isFalse);
      expect(isValidMMNamespace('im.vector'), isFalse);
      expect(isValidMMNamespace('io.element'), isFalse);
      // Only whole labels are reserved.
      expect(isValidMMNamespace('org.matrixmedia'), isTrue);
      expect(isValidMMNamespace('im.vectorx'), isTrue);
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
        'write': 'org.example.media',
        'read': 'org.example.media',
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

    test('at most eight non-built-in entries are kept; built-ins do not '
        'count (brief 7.2)', () {
      final a = MMEventNamespaceAnnouncement.tryParse(
        _block(
          read: [
            for (final ns in kMMBuiltInNamespaces) _entry(ns),
            for (var i = 0; i < 12; i++) _entry('org.example.n$i'),
          ],
        ),
      )!;
      expect(a.read, hasLength(3 + kMMMaxAnnouncedNamespaces));
      expect(
        a.read
            .where((e) => !MMEventNamespaces.isBuiltIn(e.namespace))
            .last
            .namespace,
        'org.example.n7',
      );
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

    test('write falls back to legacy when not on the read list', () {
      final a = MMEventNamespaceAnnouncement.tryParse(
        _block(write: 'org.unlisted.media', read: [_entry('com.matrixmedia')]),
      )!;
      expect(a.writesLegacy, isTrue);
    });

    test('a built-in write is always on the read list', () {
      final a = MMEventNamespaceAnnouncement.tryParse(
        _block(write: 'dev.matrixmedia', read: [_entry('com.matrixmedia')]),
      )!;
      expect(a.writeNamespace, 'dev.matrixmedia');
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

    test('bot_user_id must be @localpart:server, at most 255 characters '
        '(brief 7.8)', () {
      final long = '@${'a' * 240}:example.org'; // 253 characters
      expect(
        MMEventNamespaceAnnouncement.tryParse(_block(bot: long))!.botUserId,
        long,
      );
      for (final bad in [
        '@${'a' * 243}:example.org', // 256 characters
        '@:example.org',
        '@bot:',
        '@bot',
        'bot:example.org',
        '@bo t:example.org',
        '@bot:exa mple.org',
      ]) {
        final a = MMEventNamespaceAnnouncement.tryParse(
          _block(bot: bad, write: 'dev.matrixmedia'),
        )!;
        expect(a.botUserId, isNull, reason: bad);
        // The rest of the block still applies.
        expect(a.writeNamespace, 'dev.matrixmedia');
        expect(a.read, isNotEmpty);
      }
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
        MMEventNamespaces.builtIn.writeTypeFor(MMEventSuffix.feedPost),
        'com.steegler.matrixmedia.feed.post',
      );
    });

    test('account data: legacy write choice reads legacy first, writes '
        'both legacy first (brief 7.7)', () {
      expect(mmEventTypesToWrite(MMEventSuffix.feedMuted), [
        'com.steegler.matrixmedia.feed_muted',
        'dev.matrixmedia.feed_muted',
      ]);
      expect(mmAccountDataTypesToRead(MMEventSuffix.reactionRecents), [
        'com.steegler.matrixmedia.reaction_recents',
        'dev.matrixmedia.reaction_recents',
      ]);
      expect(MMEventNamespaces.builtIn.typesFor(MMEventSuffix.feedMuted), [
        'com.steegler.matrixmedia.feed_muted',
        'dev.matrixmedia.feed_muted',
      ]);
      expect(
        () => mmEventTypeToWrite(MMEventSuffix.feedMuted),
        throwsArgumentError,
      );
      expect(
        () => mmAccountDataTypesToRead(MMEventSuffix.feedPost),
        throwsArgumentError,
      );
      expect(MMEventNamespaces.builtIn.writeTypesFor(MMEventSuffix.feedPost), [
        'com.steegler.matrixmedia.feed.post',
      ]);
    });

    test('account data: a namespace write choice reads it first, then '
        'legacy, and writes both (brief 7.7)', () {
      expect(
        mmEventTypesToWrite(
          MMEventSuffix.reactionRecents,
          writeNamespace: 'org.example.media',
        ),
        [
          'org.example.media.reaction_recents',
          'com.steegler.matrixmedia.reaction_recents',
        ],
      );
      final ns = MMEventNamespaces.builtIn.merge(
        MMEventNamespaceAnnouncement.tryParse(_block(write: 'dev.matrixmedia')),
      );
      expect(ns.typesFor(MMEventSuffix.feedMuted), [
        'dev.matrixmedia.feed_muted',
        'com.steegler.matrixmedia.feed_muted',
      ]);
      // The copy read first is written first (brief 7.7, clarified).
      expect(ns.writeTypesFor(MMEventSuffix.feedMuted), [
        'dev.matrixmedia.feed_muted',
        'com.steegler.matrixmedia.feed_muted',
      ]);
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
        b.writeTypeFor(MMEventSuffix.feedPost),
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
      String write = 'legacy',
    }) => MMEventNamespaceAnnouncement.tryParse(
      _block(
        bot: bot,
        write: write,
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
        'org.example.c',
        'org.example.a',
        'org.example.b',
      ]);
    });

    test('no usable block keeps the read list and writes legacy (7.3)', () {
      final ns = MMEventNamespaces.builtIn.merge(
        announce(['org.example.a'], write: 'org.example.a'),
      );
      expect(ns.writeNamespace, 'org.example.a');
      final after = ns.merge(null);
      expect(after.readNamespaces, ns.readNamespaces);
      expect(after.writeNamespace, isNull);
      expect(
        after.writeTypeFor(MMEventSuffix.feedPost),
        'com.steegler.matrixmedia.feed.post',
      );
    });

    test('built-in names do not count against the cap (7.2)', () {
      final ns = MMEventNamespaces.builtIn.merge(
        announce([for (var i = 0; i < 8; i++) 'org.example.n$i']),
      );
      expect(ns.announcedExtras, hasLength(8));
    });

    test('the latest announcement is always kept, earlier entries fill '
        'the rest, most recent first (7.2)', () {
      var ns = MMEventNamespaces.builtIn;
      ns = ns.merge(announce(['org.example.a', 'org.example.b']));
      ns = ns.merge(announce(['org.example.c', 'org.example.d']));
      ns = ns.merge(announce([for (var i = 0; i < 5; i++) 'org.example.n$i']));
      expect(ns.announcedExtras, [
        for (var i = 0; i < 5; i++) 'org.example.n$i',
        'org.example.c',
        'org.example.d',
        'org.example.a',
      ]);
    });

    test('auditor scenario: a new write name after five mistaken names is '
        'read and written (7.2)', () {
      var ns = MMEventNamespaces.builtIn.merge(
        announce([for (var i = 0; i < 5; i++) 'org.mistake.n$i']),
      );
      ns = ns.merge(announce(['org.example.new'], write: 'org.example.new'));
      expect(ns.readNamespaces, contains('org.example.new'));
      expect(ns.writeNamespace, 'org.example.new');
      expect(
        ns.writeTypeFor(MMEventSuffix.feedPost),
        'org.example.new.feed.post',
      );
      // Even with the union full, the latest entries win the room.
      ns = MMEventNamespaces.builtIn
          .merge(announce([for (var i = 0; i < 8; i++) 'org.mistake.n$i']))
          .merge(announce(['org.example.new'], write: 'org.example.new'));
      expect(ns.announcedExtras.first, 'org.example.new');
      expect(ns.announcedExtras, hasLength(8));
      expect(ns.writeNamespace, 'org.example.new');
    });

    test('write is honoured only when on the effective read list', () {
      final unlisted = MMEventNamespaces.builtIn.merge(
        announce(['org.example.a'], write: 'org.example.zzz'),
      );
      expect(unlisted.writeNamespace, isNull);
      // An earlier entry still in the union counts.
      final earlier = MMEventNamespaces.builtIn
          .merge(announce(['org.example.a']))
          .merge(announce(['org.example.b'], write: 'org.example.a'));
      expect(earlier.writeNamespace, 'org.example.a');
      final builtInWrite = MMEventNamespaces.builtIn.merge(
        announce(const [], write: 'dev.matrixmedia'),
      );
      expect(builtInWrite.writeNamespace, 'dev.matrixmedia');
    });

    test('only the latest announced bot is kept per namespace', () {
      var ns = MMEventNamespaces.builtIn;
      for (var i = 0; i < 500; i++) {
        ns = ns.merge(announce(['org.example.a'], bot: '@bot$i:example.org'));
      }
      expect(ns.botsFor('org.example.a'), {'@bot499:example.org'});
      // After a rename, the old bot's server-written events stop counting.
      expect(
        ns.honouredMatch('org.example.a.stream', senderId: '@bot0:example.org'),
        isNull,
      );
      expect(
        ns.honouredMatch(
          'org.example.a.stream',
          senderId: '@bot499:example.org',
        ),
        isNotNull,
      );
      // An announcement without a usable bot keeps the previous one.
      final kept = ns.merge(
        MMEventNamespaceAnnouncement.tryParse(
          _block(bot: null, read: [_entry('org.example.a')]),
        ),
      );
      expect(kept.botsFor('org.example.a'), {'@bot499:example.org'});
      // Restored lists keep the first (latest) only.
      final stored = ns.toJson();
      (stored['extras'] as List).first['bots'] = [
        for (var i = 0; i < 20; i++) '@x$i:example.org',
      ];
      expect(MMEventNamespaces.fromJson(stored).botsFor('org.example.a'), {
        '@x0:example.org',
      });
      expect(ns.botsFor('dev.matrixmedia'), isEmpty);
    });

    test('persists and restores, write choice included (7.3)', () {
      final ns = MMEventNamespaces.builtIn.merge(
        announce(['org.example.a'], write: 'org.example.a'),
      );
      final restored = MMEventNamespaces.fromJson(
        jsonDecode(jsonEncode(ns.toJson())),
      );
      expect(restored.readNamespaces, ns.readNamespaces);
      expect(restored.botsFor('org.example.a'), {'@mmbot:example.org'});
      expect(restored.writeNamespace, 'org.example.a');
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
        'version': 2,
        'write': 'org.not.stored',
        'extras': [
          5,
          {'namespace': 'org.matrix', 'profile': 'v1', 'bots': []},
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
      expect(partial.writeNamespace, isNull);
    });
  });

  group('room marker block', () {
    final account = MMEventNamespaces.builtIn.merge(
      MMEventNamespaceAnnouncement.tryParse(
        _block(read: [for (var i = 0; i < 5; i++) _entry('org.example.n$i')]),
      ),
    );
    final room = MMEventNamespaceAnnouncement.tryParse(
      _block(
        bot: '@bot:other.org',
        read: [_entry('org.other.media'), _entry('org.example.n0')],
      ),
    );

    test('adds the room names even when the account list is full', () {
      final view = account.withRoomBlock(room);
      expect(view.matches('org.other.media.stream', 'stream'), isTrue);
      expect(
        view.honouredMatch(
          'org.other.media.stream',
          senderId: '@bot:other.org',
        ),
        isNotNull,
      );
      expect(
        view.honouredMatch(
          'org.other.media.stream',
          senderId: '@mmbot:example.org',
        ),
        isNull,
      );
      // A name both announced: either server's bot is accepted.
      expect(
        view.honouredMatch('org.example.n0.stream', senderId: '@bot:other.org'),
        isNotNull,
      );
      expect(
        view.honouredMatch(
          'org.example.n0.stream',
          senderId: '@mmbot:example.org',
        ),
        isNotNull,
      );
    });

    test('room names and room bots are never persisted with the account', () {
      final view = account.withRoomBlock(room);
      final restored = MMEventNamespaces.fromJson(
        jsonDecode(jsonEncode(view.toJson())),
      );
      expect(restored.readNamespaces, isNot(contains('org.other.media')));
      expect(account.readNamespaces, isNot(contains('org.other.media')));
      // org.example.n0 is on both lists: only the account's bot is kept.
      expect(restored.botsFor('org.example.n0'), {'@mmbot:example.org'});
      expect(account.botsFor('org.example.n0'), {'@mmbot:example.org'});
      expect(view.toJson(), account.toJson());
    });

    test('a room block does not change the write choice', () {
      final writing = MMEventNamespaces.builtIn.merge(
        MMEventNamespaceAnnouncement.tryParse(_block(write: 'dev.matrixmedia')),
      );
      expect(writing.withRoomBlock(room).writeNamespace, 'dev.matrixmedia');
    });

    test('no block changes nothing', () {
      expect(identical(account.withRoomBlock(null), account), isTrue);
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

  group('MMApiClient', () {
    const tokenBody = {
      'mm_token': 'mm',
      'refresh_token': 'refresh',
      'user_id': '@a:example.org',
      'expires_in': 900,
    };

    test('token exchange parses and reports event_namespaces', () async {
      final client = MockClient(
        (_) async => http.Response(
          jsonEncode({...tokenBody, 'event_namespaces': _block()}),
          200,
        ),
      );
      final api = MMApiClient(
        baseUrl: 'https://mm.example',
        httpClient: client,
      );
      final seen = <MMEventNamespaceAnnouncement?>[];
      api.onEventNamespaces = seen.add;
      final result = await api.authenticate(
        const MMOpenIdToken(
          accessToken: 't',
          matrixServerName: 'example.org',
          expiresIn: 3600,
        ),
      );
      expect(result.eventNamespaces, isNotNull);
      expect(result.eventNamespaces!.botUserId, '@mmbot:example.org');
      expect(api.eventNamespaces, same(result.eventNamespaces));
      expect(seen, [same(result.eventNamespaces)]);
    });

    test('a response without the block works and reports null', () async {
      final client = MockClient(
        (_) async => http.Response(jsonEncode(tokenBody), 200),
      );
      final api = MMApiClient(
        baseUrl: 'https://mm.example',
        httpClient: client,
      );
      final seen = <MMEventNamespaceAnnouncement?>[];
      api.onEventNamespaces = seen.add;
      final result = await api.authenticate(
        const MMOpenIdToken(
          accessToken: 't',
          matrixServerName: 'example.org',
          expiresIn: 3600,
        ),
      );
      expect(result.mmToken, 'mm');
      expect(result.eventNamespaces, isNull);
      expect(seen, [null]);
    });

    test('a malformed block does not fail the exchange', () async {
      final client = MockClient(
        (_) async => http.Response(
          jsonEncode({...tokenBody, 'event_namespaces': 'garbage'}),
          200,
        ),
      );
      final api = MMApiClient(
        baseUrl: 'https://mm.example',
        httpClient: client,
      );
      final result = await api.authenticate(
        const MMOpenIdToken(
          accessToken: 't',
          matrixServerName: 'example.org',
          expiresIn: 3600,
        ),
      );
      expect(result.mmToken, 'mm');
      expect(result.eventNamespaces, isNull);
    });

    test('refresh parses and reports event_namespaces', () async {
      final client = MockClient(
        (_) async => http.Response(
          jsonEncode({
            ...tokenBody,
            'event_namespaces': _block(write: 'dev.matrixmedia'),
          }),
          200,
        ),
      );
      final api = MMApiClient(baseUrl: 'https://mm.example', httpClient: client)
        ..setToken('old', 'refresh');
      final seen = <MMEventNamespaceAnnouncement?>[];
      api.onEventNamespaces = seen.add;
      final result = await api.refreshSession();
      expect(result.eventNamespaces!.writeNamespace, 'dev.matrixmedia');
      expect(seen.single!.writeNamespace, 'dev.matrixmedia');
    });

    test('sends X-MM-Client when set, and nothing when not', () async {
      final headers = <Map<String, String>>[];
      final client = MockClient((request) async {
        headers.add(request.headers);
        return http.Response('{}', 200);
      });
      final api = MMApiClient(
        baseUrl: 'https://mm.example',
        httpClient: client,
        clientHeader: 'matrixmedia-fluffychat/2.4.0 (web; build 3546)',
      )..setToken('mm', 'refresh');
      await api.getCreatorDefaults();
      api.clientHeader = null;
      await api.getCreatorDefaults();
      expect(
        headers[0]['X-MM-Client'],
        'matrixmedia-fluffychat/2.4.0 (web; build 3546)',
      );
      expect(headers[1].containsKey('X-MM-Client'), isFalse);
    });

    test('a throwing listener does not fail the exchange', () async {
      final client = MockClient(
        (_) async => http.Response(
          jsonEncode({...tokenBody, 'event_namespaces': _block()}),
          200,
        ),
      );
      final api = MMApiClient(baseUrl: 'https://mm.example', httpClient: client)
        ..onEventNamespaces = (_) => throw StateError('listener bug');
      final result = await api.authenticate(
        const MMOpenIdToken(
          accessToken: 't',
          matrixServerName: 'example.org',
          expiresIn: 3600,
        ),
      );
      expect(result.mmToken, 'mm');
      expect(api.isAuthenticated, isTrue);
      expect(api.eventNamespaces, isNotNull);
    });

    test('the token exchange waits for a pending header (7.5)', () async {
      final headers = <Map<String, String>>[];
      final client = MockClient((request) async {
        headers.add(request.headers);
        return http.Response(jsonEncode(tokenBody), 200);
      });
      final header = Completer<String?>();
      final api = MMApiClient(
        baseUrl: 'https://mm.example',
        httpClient: client,
        pendingClientHeader: header.future,
      );
      final exchange = api.authenticate(
        const MMOpenIdToken(
          accessToken: 't',
          matrixServerName: 'example.org',
          expiresIn: 3600,
        ),
      );
      await Future<void>.delayed(Duration.zero);
      expect(headers, isEmpty);
      header.complete('matrixmedia-fluffychat/2.4.0 (ios; build 1)');
      await exchange;
      expect(
        headers.single['X-MM-Client'],
        'matrixmedia-fluffychat/2.4.0 (ios; build 1)',
      );
      expect(api.clientHeader, 'matrixmedia-fluffychat/2.4.0 (ios; build 1)');
    });

    test('a header that never resolves holds the request at most 2 s '
        '(7.5, amended)', () async {
      final headers = <Map<String, String>>[];
      final client = MockClient((request) async {
        headers.add(request.headers);
        return http.Response(jsonEncode(tokenBody), 200);
      });
      final never = Completer<String?>();
      final api = MMApiClient(
        baseUrl: 'https://mm.example',
        httpClient: client,
        pendingClientHeader: never.future,
      );
      const token = MMOpenIdToken(
        accessToken: 't',
        matrixServerName: 'example.org',
        expiresIn: 3600,
      );
      final watch = Stopwatch()..start();
      // Two concurrent requests share the one bounded wait.
      await Future.wait([api.authenticate(token), api.authenticate(token)]);
      expect(watch.elapsed, lessThan(const Duration(milliseconds: 2900)));
      expect(watch.elapsed, greaterThanOrEqualTo(const Duration(seconds: 2)));
      expect(headers, hasLength(2));
      expect(headers.every((h) => !h.containsKey('X-MM-Client')), isTrue);

      // Later requests do not wait again.
      watch.reset();
      await api.authenticate(token);
      expect(watch.elapsed, lessThan(const Duration(milliseconds: 500)));

      // A late answer is used from then on.
      never.complete('matrixmedia-fluffychat/2.4.0 (web; build 3)');
      await Future<void>.delayed(Duration.zero);
      await api.authenticate(token);
      expect(
        headers.last['X-MM-Client'],
        'matrixmedia-fluffychat/2.4.0 (web; build 3)',
      );
    });

    test('a failed pending header still sends the request', () async {
      final headers = <Map<String, String>>[];
      final client = MockClient((request) async {
        headers.add(request.headers);
        return http.Response(jsonEncode(tokenBody), 200);
      });
      final api = MMApiClient(
        baseUrl: 'https://mm.example',
        httpClient: client,
        pendingClientHeader: Future<String?>.error(StateError('no version')),
      );
      await api.authenticate(
        const MMOpenIdToken(
          accessToken: 't',
          matrixServerName: 'example.org',
          expiresIn: 3600,
        ),
      );
      expect(headers.single.containsKey('X-MM-Client'), isFalse);
    });

    test('stream info carries state_event_id when present', () {
      final info = MMStreamInfo.fromJson({
        'stream_id': 's1',
        'room_id': '!r:x',
        'state_event_id': r'$legacy',
      });
      expect(info.stateEventId, r'$legacy');
      expect(MMStreamInfo.fromJson({'stream_id': 's1'}).stateEventId, isNull);
    });

    group('X-MM-Client on the web (brief 8.4)', () {
      final page = Uri.parse('https://app.example.org/web/#/rooms');

      test('the rule', () {
        bool may(String target, {bool web = true, bool optIn = false}) =>
            mmMaySendClientHeader(
              Uri.parse(target),
              isWeb: web,
              page: page,
              crossOriginOptIn: optIn,
            );
        expect(may('https://app.example.org/_mm/client/v1/x'), isTrue);
        expect(may('https://mm.example.org/_mm/client/v1/x'), isFalse);
        expect(may('http://app.example.org/x'), isFalse);
        expect(may('https://app.example.org:8443/x'), isFalse);
        expect(may('https://mm.example.org/x', optIn: true), isTrue);
        expect(may('https://mm.example.org/x', web: false), isTrue);
      });

      Future<Map<String, String>> sent({
        required String baseUrl,
        required bool web,
        bool optIn = false,
      }) async {
        late Map<String, String> headers;
        final api =
            MMApiClient(
                baseUrl: baseUrl,
                httpClient: MockClient((request) async {
                  headers = request.headers;
                  return http.Response('{}', 200);
                }),
                clientHeader: 'matrixmedia-fluffychat/2.4.0 (web; build 1)',
                sendClientHeaderCrossOrigin: optIn,
              )
              ..debugIsWeb = web
              ..debugPageUri = page
              ..setToken('mm', 'refresh');
        await api.getCreatorDefaults();
        return headers;
      }

      test('same origin on the web: sent', () async {
        final h = await sent(baseUrl: 'https://app.example.org', web: true);
        expect(h['X-MM-Client'], isNotNull);
      });

      test(
        'cross-origin on the web: not sent, the request still goes',
        () async {
          final h = await sent(baseUrl: 'https://mm.example.org', web: true);
          expect(h.containsKey('X-MM-Client'), isFalse);
          expect(h['Authorization'], 'Bearer mm');
        },
      );

      test('cross-origin on the web with the opt-in: sent', () async {
        final h = await sent(
          baseUrl: 'https://mm.example.org',
          web: true,
          optIn: true,
        );
        expect(h['X-MM-Client'], isNotNull);
      });

      test('other platforms: always sent', () async {
        final h = await sent(baseUrl: 'https://mm.example.org', web: false);
        expect(h['X-MM-Client'], isNotNull);
      });
    });

    test('MMClient passes the header through', () {
      final c = MMClient(
        serverUrl: 'https://mm.example',
        clientHeader: 'matrixmedia-fluffychat/1 (ios; build 2)',
      );
      expect(c.api.clientHeader, 'matrixmedia-fluffychat/1 (ios; build 2)');
      c.clientHeader = null;
      expect(c.api.clientHeader, isNull);
      expect(c.eventNamespaces, isNull);
      c.dispose();
    });
  });
}
