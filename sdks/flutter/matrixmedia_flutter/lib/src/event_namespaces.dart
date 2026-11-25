/// MatrixMedia Matrix event namespaces.
///
/// MatrixMedia's custom Matrix event types are a namespace plus a fixed
/// suffix (`<namespace>.stream`, `<namespace>.feed.post`, ...). Every client
/// reads a built-in list of namespaces; the mm-core server may announce more
/// in the `event_namespaces` block of its token-exchange response, and says
/// which namespace a client uses for the events it writes itself.
///
/// This file holds the suffix table, the namespace validation rules, the
/// parser for the announced block, and a matcher that recognises an event
/// type under any namespace on the effective read list.
library;

/// The namespace new event types are defined under.
const String kMMCurrentNamespace = 'dev.matrixmedia';

/// Namespaces every client reads, always. An announcement can add to this
/// list; it can never remove an entry.
const List<String> kMMBuiltInNamespaces = [
  'dev.matrixmedia',
  'com.matrixmedia',
  'com.steegler.matrixmedia',
];

/// Payload profiles this client can parse. One payload format exists today.
const Set<String> kMMKnownProfiles = {'v1'};

/// The `write` value that means "use the legacy names".
const String kMMLegacyWrite = 'legacy';

/// At most this many namespaces that are not built in are kept, both from
/// one announcement and in the stored union. Built-in names never count.
const int kMMMaxAnnouncedNamespaces = 8;

/// Longest namespace accepted from an announcement.
const int kMMMaxNamespaceLength = 64;

/// The protocol's event suffixes and their legacy full names.
abstract final class MMEventSuffix {
  /// State, key `""`.
  static const String stream = 'stream';

  /// State, key = stream id.
  static const String streamE2eeKey = 'stream.e2ee_key';

  /// State, key `""`. The room marker; never had a legacy name on the wire.
  static const String roomConfig = 'room_config';

  /// Timeline.
  static const String donation = 'donation';

  /// Timeline, written by the server.
  static const String feedBroadcastStarted = 'feed.broadcast.started';

  /// Timeline, written by the server.
  static const String feedBroadcastEnded = 'feed.broadcast.ended';

  /// Timeline, written by the server.
  static const String feedRecordingAvailable = 'feed.recording.available';

  /// Timeline, written by clients.
  static const String feedPost = 'feed.post';

  /// Room account data, written by clients.
  static const String feedMuted = 'feed_muted';

  /// Global account data, written by clients.
  static const String reactionRecents = 'reaction_recents';

  /// State, defined, never sent.
  static const String subscriptionTiers = 'subscription_tiers';

  /// State, defined, never sent.
  static const String subscriptionProof = 'subscription_proof';

  /// State, defined, never sent.
  static const String contentGate = 'content_gate';

  /// Legacy full name per suffix. `room_config` has none.
  static const Map<String, String> legacyNames = {
    stream: 'com.matrixmedia.stream',
    streamE2eeKey: 'com.matrixmedia.stream.e2ee_key',
    donation: 'com.matrixmedia.donation',
    feedBroadcastStarted: 'com.steegler.matrixmedia.feed.broadcast.started',
    feedBroadcastEnded: 'com.steegler.matrixmedia.feed.broadcast.ended',
    feedRecordingAvailable: 'com.steegler.matrixmedia.feed.recording.available',
    feedPost: 'com.steegler.matrixmedia.feed.post',
    feedMuted: 'com.steegler.matrixmedia.feed_muted',
    reactionRecents: 'com.steegler.matrixmedia.reaction_recents',
    subscriptionTiers: 'com.matrixmedia.subscription_tiers',
    subscriptionProof: 'com.matrixmedia.subscription_proof',
    contentGate: 'com.matrixmedia.content_gate',
  };

  /// Every suffix the protocol defines. Nothing else ever matches.
  static const Set<String> all = {
    stream,
    streamE2eeKey,
    roomConfig,
    donation,
    feedBroadcastStarted,
    feedBroadcastEnded,
    feedRecordingAvailable,
    feedPost,
    feedMuted,
    reactionRecents,
    subscriptionTiers,
    subscriptionProof,
    contentGate,
  };

  /// Suffixes only the server's bot sends. Under a namespace that is not
  /// built in, an event with one of these suffixes is honoured only when
  /// its sender is the announced bot. The three never-sent state types are
  /// included: their only defined sender is mm-core.
  static const Set<String> serverWritten = {
    stream,
    streamE2eeKey,
    roomConfig,
    donation,
    feedBroadcastStarted,
    feedBroadcastEnded,
    feedRecordingAvailable,
    subscriptionTiers,
    subscriptionProof,
    contentGate,
  };

  /// Account-data suffixes. Always written under two names and read with a
  /// fallback, in an order set by the write choice (brief 7.7); see
  /// [mmAccountDataTypesToRead] and [mmEventTypesToWrite].
  static const Set<String> accountData = {feedMuted, reactionRecents};

  /// The legacy full name of [suffix], or null when it has none.
  static String? legacyName(String suffix) => legacyNames[suffix];
}

const List<String> _reservedNamespacePrefixes = [
  'm.',
  'org.matrix.',
  'im.vector.',
  'io.element.',
];

final RegExp _namespacePattern = RegExp(r'^[a-z0-9-]+(\.[a-z0-9-]+)+$');

/// True when [namespace] may be taken from an announcement: lower-case
/// reverse-DNS with at least two labels of `[a-z0-9-]`, at most 64
/// characters, and `<namespace>.` does not start with `m.`, `org.matrix.`,
/// `im.vector.` or `io.element.` (so `m`, `org.matrix`, `im.vector` and
/// `io.element` themselves are rejected too).
bool isValidMMNamespace(String namespace) {
  if (namespace.isEmpty || namespace.length > kMMMaxNamespaceLength) {
    return false;
  }
  final dotted = '$namespace.';
  for (final prefix in _reservedNamespacePrefixes) {
    if (dotted.startsWith(prefix)) return false;
  }
  return _namespacePattern.hasMatch(namespace);
}

bool _isBuiltIn(String namespace) => kMMBuiltInNamespaces.contains(namespace);

/// Longest user id accepted from an announcement (brief 7.8).
const int kMMMaxUserIdLength = 255;

final RegExp _userIdPattern = RegExp(r'^@[^:\s]+:\S+$');

/// True when [userId] has the form `@localpart:server` and is at most
/// [kMMMaxUserIdLength] characters (brief 7.8).
bool _looksLikeUserId(String userId) =>
    userId.length <= kMMMaxUserIdLength && _userIdPattern.hasMatch(userId);

/// The server part of a Matrix user id, or null when it is not one.
String? mmUserIdServer(String userId) {
  if (!_looksLikeUserId(userId)) return null;
  return userId.substring(userId.indexOf(':') + 1);
}

/// One namespace entry of an announcement.
class MMAnnouncedNamespace {
  final String namespace;
  final String profile;

  const MMAnnouncedNamespace({required this.namespace, required this.profile});

  Map<String, dynamic> toJson() => {'namespace': namespace, 'profile': profile};

  @override
  bool operator ==(Object other) =>
      other is MMAnnouncedNamespace &&
      other.namespace == namespace &&
      other.profile == profile;

  @override
  int get hashCode => Object.hash(namespace, profile);

  @override
  String toString() => '$namespace ($profile)';
}

/// The `event_namespaces` block mm-core adds to `POST /auth/token` and
/// `/auth/refresh` (and the room marker carries for its room).
///
/// Only the valid part is kept: invalid read entries are dropped and at
/// most [kMMMaxAnnouncedNamespaces] entries that are not built in are kept
/// (built-in entries never count).
class MMEventNamespaceAnnouncement {
  /// The only block version this client understands.
  static const int supportedVersion = 1;

  final int version;

  /// The `write` value as announced when it names a valid namespace, else
  /// null (`"legacy"`, missing or invalid). Whether it is honoured depends
  /// on the read list it is resolved against; see [writeNamespace] and
  /// [MMEventNamespaces.merge].
  final String? requestedWrite;

  /// The valid read entries, in announcement order.
  final List<MMAnnouncedNamespace> read;

  /// The announcing server's bot, when it gave a well-formed user id.
  final String? botUserId;

  const MMEventNamespaceAnnouncement({
    this.version = supportedVersion,
    this.requestedWrite,
    this.read = const [],
    this.botUserId,
  });

  /// The namespace to write under when this announcement is taken alone:
  /// [requestedWrite] if it is built in or one of [read], else null for the
  /// legacy names. A client that keeps a union uses
  /// [MMEventNamespaces.writeNamespace] instead.
  String? get writeNamespace {
    final w = requestedWrite;
    if (w == null) return null;
    if (_isBuiltIn(w) || read.any((e) => e.namespace == w)) return w;
    return null;
  }

  /// True when client-written events use the legacy names.
  bool get writesLegacy => writeNamespace == null;

  /// Parse a block. Returns null, without throwing, for a missing or
  /// malformed block or one with an unknown version.
  static MMEventNamespaceAnnouncement? tryParse(Object? json) {
    if (json is! Map) return null;
    final version = json['version'];
    if (version is! int || version != supportedVersion) return null;

    final read = <MMAnnouncedNamespace>[];
    final seen = <String>{};
    var extras = 0;
    final rawRead = json['read'];
    if (rawRead is List) {
      for (final entry in rawRead) {
        if (entry is! Map) continue;
        final namespace = entry['namespace'];
        final profile = entry['profile'];
        if (namespace is! String || profile is! String) continue;
        if (!kMMKnownProfiles.contains(profile)) continue;
        if (!isValidMMNamespace(namespace)) continue;
        if (seen.contains(namespace)) continue;
        if (!_isBuiltIn(namespace)) {
          if (extras >= kMMMaxAnnouncedNamespaces) continue;
          extras++;
        }
        seen.add(namespace);
        read.add(MMAnnouncedNamespace(namespace: namespace, profile: profile));
      }
    }

    final rawWrite = json['write'];
    final requestedWrite =
        rawWrite is String &&
            rawWrite != kMMLegacyWrite &&
            isValidMMNamespace(rawWrite)
        ? rawWrite
        : null;

    final rawBot = json['bot_user_id'];
    final botUserId = rawBot is String && _looksLikeUserId(rawBot)
        ? rawBot
        : null;

    return MMEventNamespaceAnnouncement(
      version: version,
      requestedWrite: requestedWrite,
      read: List.unmodifiable(read),
      botUserId: botUserId,
    );
  }

  /// The full event type to write for [suffix] under this announcement
  /// alone. See [mmEventTypeToWrite].
  String writeTypeFor(String suffix) =>
      mmEventTypeToWrite(suffix, writeNamespace: writeNamespace);

  Map<String, dynamic> toJson() => {
    'version': version,
    'write': requestedWrite ?? kMMLegacyWrite,
    'read': [for (final r in read) r.toJson()],
    if (botUserId != null) 'bot_user_id': botUserId,
  };
}

/// The event type a client writes for a client-written [suffix]:
/// `<writeNamespace>.<suffix>`, or the legacy name when [writeNamespace] is
/// null. A suffix with no legacy name (`room_config`) uses the current
/// namespace.
///
/// Account data is written under two names; asking for one of those
/// suffixes here throws [ArgumentError]. Use [mmEventTypesToWrite].
String mmEventTypeToWrite(String suffix, {String? writeNamespace}) {
  if (MMEventSuffix.accountData.contains(suffix)) {
    throw ArgumentError.value(
      suffix,
      'suffix',
      'account data is written under two names; use mmEventTypesToWrite',
    );
  }
  if (writeNamespace != null) return '$writeNamespace.$suffix';
  return MMEventSuffix.legacyName(suffix) ?? '$kMMCurrentNamespace.$suffix';
}

/// Every type a client writes for [suffix], in the order to write them.
/// Account data (`feed_muted`, `reaction_recents`) goes under two names,
/// legacy first (brief 7.7): with a legacy write choice the legacy name and
/// `dev.matrixmedia.<suffix>`, with a namespace the legacy name and
/// `<namespace>.<suffix>`. A failure of the second write should be logged
/// and not fail the operation. Anything else is the one type
/// [mmEventTypeToWrite] gives.
List<String> mmEventTypesToWrite(String suffix, {String? writeNamespace}) {
  if (MMEventSuffix.accountData.contains(suffix)) {
    final legacy = MMEventSuffix.legacyName(suffix)!;
    return [legacy, '${writeNamespace ?? kMMCurrentNamespace}.$suffix'];
  }
  return [mmEventTypeToWrite(suffix, writeNamespace: writeNamespace)];
}

/// The account-data types to read for [suffix], in order: the first that
/// exists is the value (brief 7.7). With a legacy write choice (null) the
/// legacy copy is the truth: legacy first, then `dev.matrixmedia.<suffix>`.
/// With a namespace: `<namespace>.<suffix>` first, then legacy.
List<String> mmAccountDataTypesToRead(String suffix, {String? writeNamespace}) {
  if (!MMEventSuffix.accountData.contains(suffix)) {
    throw ArgumentError.value(suffix, 'suffix', 'not an account-data suffix');
  }
  final legacy = MMEventSuffix.legacyName(suffix)!;
  if (writeNamespace == null) return [legacy, '$kMMCurrentNamespace.$suffix'];
  return ['$writeNamespace.$suffix', legacy];
}

/// A recognised MatrixMedia event type.
class MMEventTypeMatch {
  final String namespace;
  final String suffix;

  /// True when [namespace] is on the built-in list.
  final bool builtIn;

  const MMEventTypeMatch({
    required this.namespace,
    required this.suffix,
    required this.builtIn,
  });

  @override
  String toString() => 'MMEventTypeMatch($namespace, $suffix)';
}

class _Extra {
  final String profile;
  final Set<String> bots;
  _Extra(this.profile, this.bots);
  _Extra copy() => _Extra(profile, {...bots});
}

/// The effective read list of one account, and its write choice: the
/// built-in namespaces plus the announced ones kept under the rules of
/// brief 7.2, with the bots that announced them.
///
/// Immutable. A room view made with [withRoomBlock] adds one room's marker
/// entries for reading that room; they are never part of [toJson].
class MMEventNamespaces {
  /// Account entries that are not built in, most recently announced first.
  final Map<String, _Extra> _account;

  /// Entries from one room's marker block (room views only).
  final Map<String, _Extra> _room;

  /// The honoured write namespace, or null for the legacy names.
  final String? writeNamespace;

  MMEventNamespaces._(this._account, this._room, this.writeNamespace);

  /// The built-in list alone, writing legacy names. What a client uses
  /// when nothing was announced.
  static final MMEventNamespaces builtIn = MMEventNamespaces._(
    const {},
    const {},
    null,
  );

  /// Every namespace read: built-in ones first, then the account's
  /// (most recently announced first), then the room's.
  List<String> get readNamespaces => [
    ...kMMBuiltInNamespaces,
    ..._account.keys,
    for (final ns in _room.keys)
      if (!_account.containsKey(ns)) ns,
  ];

  /// The account's namespaces that are not built in, most recent first.
  List<String> get announcedExtras => List.unmodifiable(_account.keys);

  /// The bots allowed to send server-written events under [namespace]:
  /// the account's and, in a room view, the room's. Empty for built-in
  /// namespaces (no sender rule) and unknown ones.
  Set<String> botsFor(String namespace) => Set.unmodifiable({
    ...?_account[namespace]?.bots,
    ...?_room[namespace]?.bots,
  });

  static bool isBuiltIn(String namespace) => _isBuiltIn(namespace);

  /// The account's list after a token-exchange or refresh response
  /// (brief 7.2, 7.3):
  ///
  /// * every valid entry of [announcement] is kept;
  /// * earlier entries fill the room left, up to
  ///   [kMMMaxAnnouncedNamespaces] that are not built in, most recently
  ///   announced first;
  /// * the write choice is the announced one when it is on the resulting
  ///   read list, else legacy.
  ///
  /// A null [announcement] (no usable block) keeps the read list and sets
  /// the write choice back to legacy.
  MMEventNamespaces merge(MMEventNamespaceAnnouncement? announcement) {
    if (announcement == null) {
      return MMEventNamespaces._(_account, const {}, null);
    }
    final bot = announcement.botUserId;
    final account = <String, _Extra>{};
    for (final entry in announcement.read) {
      final ns = entry.namespace;
      if (_isBuiltIn(ns)) continue;
      final previous = _account[ns];
      account[ns] = _Extra(entry.profile, {...?previous?.bots, ?bot});
    }
    for (final e in _account.entries) {
      if (account.length >= kMMMaxAnnouncedNamespaces) break;
      account.putIfAbsent(e.key, e.value.copy);
    }
    final requested = announcement.requestedWrite;
    final write =
        requested != null &&
            (_isBuiltIn(requested) || account.containsKey(requested))
        ? requested
        : null;
    return MMEventNamespaces._(account, const {}, write);
  }

  /// This list plus the entries of one room's marker [block], for reading
  /// that room only. The caller has already decided the block is trusted.
  ///
  /// The room's entries and bots are kept apart from the account's: they do
  /// not count against the account's cap, do not change its write choice
  /// and are never written by [toJson].
  MMEventNamespaces withRoomBlock(MMEventNamespaceAnnouncement? block) {
    if (block == null || block.read.isEmpty) return this;
    final bot = block.botUserId;
    final room = <String, _Extra>{
      for (final e in _room.entries) e.key: e.value.copy(),
    };
    for (final entry in block.read) {
      final ns = entry.namespace;
      if (_isBuiltIn(ns)) continue;
      final existing = room[ns];
      if (existing != null) {
        if (bot != null) existing.bots.add(bot);
      } else {
        room[ns] = _Extra(entry.profile, {?bot});
      }
    }
    return MMEventNamespaces._(_account, room, writeNamespace);
  }

  /// Recognise [type]: it must equal `<namespace>.<suffix>` for a namespace
  /// on the read list and a suffix in [MMEventSuffix.all]. The whole string
  /// is matched; a prefix alone never matches.
  MMEventTypeMatch? match(String type) {
    for (final ns in readNamespaces) {
      final m = _matchUnder(type, ns, builtIn: _isBuiltIn(ns));
      if (m != null) return m;
    }
    return null;
  }

  static MMEventTypeMatch? _matchUnder(
    String type,
    String namespace, {
    required bool builtIn,
  }) {
    if (type.length <= namespace.length + 1) return null;
    if (!type.startsWith(namespace) || type[namespace.length] != '.') {
      return null;
    }
    final suffix = type.substring(namespace.length + 1);
    if (!MMEventSuffix.all.contains(suffix)) return null;
    return MMEventTypeMatch(
      namespace: namespace,
      suffix: suffix,
      builtIn: builtIn,
    );
  }

  /// True when [type] is `<namespace>.<suffix>` for a namespace on the
  /// read list.
  bool matches(String type, String suffix) => match(type)?.suffix == suffix;

  /// The match for [type] if the event may be honoured: under a built-in
  /// namespace always; under an announced one only when the suffix is
  /// client-written or [senderId] is a bot that announced the namespace.
  MMEventTypeMatch? honouredMatch(String type, {required String? senderId}) {
    final m = match(type);
    if (m == null) return null;
    if (m.builtIn) return m;
    if (!MMEventSuffix.serverWritten.contains(m.suffix)) return m;
    if (senderId != null && botsFor(m.namespace).contains(senderId)) {
      return m;
    }
    return null;
  }

  /// Every full type to read for [suffix], in the order to try them: the
  /// legacy name first, then the other built-in names, then announced
  /// ones. Account data follows this list's write choice
  /// ([mmAccountDataTypesToRead], brief 7.7).
  List<String> typesFor(String suffix) {
    if (MMEventSuffix.accountData.contains(suffix)) {
      return mmAccountDataTypesToRead(suffix, writeNamespace: writeNamespace);
    }
    final legacy = MMEventSuffix.legacyName(suffix);
    return [
      ?legacy,
      for (final ns in readNamespaces)
        if ('$ns.$suffix' != legacy) '$ns.$suffix',
    ];
  }

  /// The type to write for a client-written [suffix] under this list's
  /// write choice. See [mmEventTypeToWrite]; account data throws.
  String writeTypeFor(String suffix) =>
      mmEventTypeToWrite(suffix, writeNamespace: writeNamespace);

  /// Every type to write for [suffix]. See [mmEventTypesToWrite].
  List<String> writeTypesFor(String suffix) =>
      mmEventTypesToWrite(suffix, writeNamespace: writeNamespace);

  /// Serialise the account's list and write choice for persistence across
  /// restarts. Room entries and room bots are never included.
  Map<String, dynamic> toJson() => {
    'version': 2,
    'write': writeNamespace ?? kMMLegacyWrite,
    'extras': [
      for (final e in _account.entries)
        {
          'namespace': e.key,
          'profile': e.value.profile,
          'bots': e.value.bots.toList(),
        },
    ],
  };

  /// Restore from [toJson]. Anything malformed or invalid is dropped; a
  /// fully malformed value gives [builtIn].
  static MMEventNamespaces fromJson(Object? json) {
    if (json is! Map || json['version'] != 2) return builtIn;
    final account = <String, _Extra>{};
    final rawExtras = json['extras'];
    if (rawExtras is List) {
      for (final e in rawExtras) {
        if (account.length >= kMMMaxAnnouncedNamespaces) break;
        if (e is! Map) continue;
        final ns = e['namespace'];
        final profile = e['profile'];
        if (ns is! String || profile is! String) continue;
        if (!isValidMMNamespace(ns) || _isBuiltIn(ns)) continue;
        if (account.containsKey(ns)) continue;
        if (!kMMKnownProfiles.contains(profile)) continue;
        final rawBots = e['bots'];
        account[ns] = _Extra(profile, {
          if (rawBots is List)
            for (final b in rawBots)
              if (b is String && _looksLikeUserId(b)) b,
        });
      }
    }
    final rawWrite = json['write'];
    final write =
        rawWrite is String &&
            isValidMMNamespace(rawWrite) &&
            (_isBuiltIn(rawWrite) || account.containsKey(rawWrite))
        ? rawWrite
        : null;
    return MMEventNamespaces._(account, const {}, write);
  }
}

/// Format the `X-MM-Client` header value:
/// `<client-id>/<version> (<platform>; build <build>)`, for example
/// `matrixmedia-fluffychat/2.4.0 (android; build 3546)`.
///
/// Characters outside `[A-Za-z0-9._+-]` are removed from each part so the
/// value always has the documented shape.
String formatMMClientHeader({
  required String clientId,
  required String version,
  required String platform,
  required String build,
}) {
  String clean(String s) {
    final v = s.replaceAll(RegExp(r'[^A-Za-z0-9._+\-]'), '');
    return v.isEmpty ? 'unknown' : v;
  }

  return '${clean(clientId)}/${clean(version)} '
      '(${clean(platform)}; build ${clean(build)})';
}
