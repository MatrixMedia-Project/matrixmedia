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

/// At most this many announced namespace entries are kept.
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
/// characters, not `m` and not under `m.`, `org.matrix.`, `im.vector.` or
/// `io.element.`.
bool isValidMMNamespace(String namespace) {
  if (namespace.isEmpty || namespace.length > kMMMaxNamespaceLength) {
    return false;
  }
  if (namespace == 'm') return false;
  for (final prefix in _reservedNamespacePrefixes) {
    if (namespace.startsWith(prefix)) return false;
  }
  return _namespacePattern.hasMatch(namespace);
}

/// True when [userId] looks like a Matrix user id (`@local:server`).
bool _looksLikeUserId(String userId) =>
    userId.length > 3 &&
    userId.startsWith('@') &&
    userId.indexOf(':') > 1 &&
    !userId.endsWith(':');

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
/// Only the valid part is kept: invalid read entries are dropped, at most
/// [kMMMaxAnnouncedNamespaces] are kept, and a `write` value that is
/// neither `"legacy"` nor a kept read entry falls back to legacy.
class MMEventNamespaceAnnouncement {
  /// The only block version this client understands.
  static const int supportedVersion = 1;

  final int version;

  /// The namespace to write client-written events under, or null for the
  /// legacy names.
  final String? writeNamespace;

  /// The valid read entries, in announcement order.
  final List<MMAnnouncedNamespace> read;

  /// The announcing server's bot, when it gave a well-formed user id.
  final String? botUserId;

  const MMEventNamespaceAnnouncement({
    this.version = supportedVersion,
    this.writeNamespace,
    this.read = const [],
    this.botUserId,
  });

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
    final rawRead = json['read'];
    if (rawRead is List) {
      for (final entry in rawRead) {
        if (read.length >= kMMMaxAnnouncedNamespaces) break;
        if (entry is! Map) continue;
        final namespace = entry['namespace'];
        final profile = entry['profile'];
        if (namespace is! String || profile is! String) continue;
        if (!kMMKnownProfiles.contains(profile)) continue;
        if (!isValidMMNamespace(namespace)) continue;
        if (!seen.add(namespace)) continue;
        read.add(MMAnnouncedNamespace(namespace: namespace, profile: profile));
      }
    }

    final rawWrite = json['write'];
    String? writeNamespace;
    if (rawWrite is String &&
        rawWrite != kMMLegacyWrite &&
        seen.contains(rawWrite)) {
      writeNamespace = rawWrite;
    }

    final rawBot = json['bot_user_id'];
    final botUserId = rawBot is String && _looksLikeUserId(rawBot)
        ? rawBot
        : null;

    return MMEventNamespaceAnnouncement(
      version: version,
      writeNamespace: writeNamespace,
      read: List.unmodifiable(read),
      botUserId: botUserId,
    );
  }

  /// The full event type to write for [suffix]: `<write>.<suffix>`, or the
  /// legacy name when the write choice is legacy.
  String writeTypeFor(String suffix) =>
      mmEventTypeToWrite(suffix, announcement: this);

  Map<String, dynamic> toJson() => {
    'version': version,
    'write': writeNamespace ?? kMMLegacyWrite,
    'read': [for (final r in read) r.toJson()],
    if (botUserId != null) 'bot_user_id': botUserId,
  };
}

/// The event type a client writes for [suffix] given the server's
/// [announcement]. With no announcement, or a legacy write choice, this is
/// the legacy name. A suffix with no legacy name (`room_config`) uses the
/// current namespace.
String mmEventTypeToWrite(
  String suffix, {
  MMEventNamespaceAnnouncement? announcement,
}) {
  final namespace = announcement?.writeNamespace;
  if (namespace != null) return '$namespace.$suffix';
  return MMEventSuffix.legacyName(suffix) ?? '$kMMCurrentNamespace.$suffix';
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

class _ExtraNamespace {
  final String profile;
  final Set<String> bots;
  _ExtraNamespace(this.profile, this.bots);
}

/// The effective read list: the built-in namespaces plus every valid
/// announced entry this client has seen for one account, with the bots
/// that announced them.
///
/// Immutable. [merge] returns the union; nothing is ever removed, so a
/// later, shorter announcement does not shrink the list.
class MMEventNamespaces {
  /// Non-built-in namespaces in first-seen order.
  final Map<String, _ExtraNamespace> _extras;

  /// Every announced entry kept, built-in names included, for the cap.
  final List<String> _announced;

  MMEventNamespaces._(this._extras, this._announced);

  /// The built-in list alone. What a client uses when nothing was
  /// announced.
  static final MMEventNamespaces builtIn = MMEventNamespaces._(
    const {},
    const [],
  );

  /// Every namespace read, built-in ones first.
  List<String> get readNamespaces => [...kMMBuiltInNamespaces, ..._extras.keys];

  /// Namespaces learned from announcements that are not built in.
  List<String> get announcedExtras => List.unmodifiable(_extras.keys);

  /// The bots allowed to send server-written events under [namespace].
  /// Empty for built-in namespaces (no sender rule) and unknown ones.
  Set<String> botsFor(String namespace) =>
      Set.unmodifiable(_extras[namespace]?.bots ?? const <String>{});

  static bool isBuiltIn(String namespace) =>
      kMMBuiltInNamespaces.contains(namespace);

  /// The union of this list and the valid entries of [announcement].
  MMEventNamespaces merge(MMEventNamespaceAnnouncement? announcement) {
    if (announcement == null) return this;
    final extras = <String, _ExtraNamespace>{
      for (final e in _extras.entries)
        e.key: _ExtraNamespace(e.value.profile, {...e.value.bots}),
    };
    final announced = [..._announced];
    final bot = announcement.botUserId;
    for (final entry in announcement.read) {
      final ns = entry.namespace;
      if (!announced.contains(ns)) {
        if (announced.length >= kMMMaxAnnouncedNamespaces) continue;
        announced.add(ns);
      }
      if (isBuiltIn(ns)) continue;
      final existing = extras[ns];
      if (existing != null) {
        if (bot != null) existing.bots.add(bot);
      } else {
        extras[ns] = _ExtraNamespace(entry.profile, {?bot});
      }
    }
    return MMEventNamespaces._(extras, announced);
  }

  /// Recognise [type]: it must equal `<namespace>.<suffix>` for a namespace
  /// on the read list and a suffix in [MMEventSuffix.all]. The whole string
  /// is matched; a prefix alone never matches.
  MMEventTypeMatch? match(String type) {
    for (final ns in kMMBuiltInNamespaces) {
      final m = _matchUnder(type, ns, builtIn: true);
      if (m != null) return m;
    }
    for (final ns in _extras.keys) {
      final m = _matchUnder(type, ns, builtIn: false);
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
    if (senderId != null && _extras[m.namespace]!.bots.contains(senderId)) {
      return m;
    }
    return null;
  }

  /// Every full type to read for [suffix], legacy name first, then the
  /// other built-in names, then announced ones.
  List<String> typesFor(String suffix) {
    final legacy = MMEventSuffix.legacyName(suffix);
    return [
      ?legacy,
      for (final ns in readNamespaces)
        if ('$ns.$suffix' != legacy) '$ns.$suffix',
    ];
  }

  /// Serialise for persistence across restarts.
  Map<String, dynamic> toJson() => {
    'version': 1,
    'announced': _announced,
    'extras': [
      for (final e in _extras.entries)
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
    if (json is! Map || json['version'] != 1) return builtIn;
    final announced = <String>[];
    final rawAnnounced = json['announced'];
    if (rawAnnounced is List) {
      for (final ns in rawAnnounced) {
        if (announced.length >= kMMMaxAnnouncedNamespaces) break;
        if (ns is String && isValidMMNamespace(ns) && !announced.contains(ns)) {
          announced.add(ns);
        }
      }
    }
    final extras = <String, _ExtraNamespace>{};
    final rawExtras = json['extras'];
    if (rawExtras is List) {
      for (final e in rawExtras) {
        if (e is! Map) continue;
        final ns = e['namespace'];
        final profile = e['profile'];
        if (ns is! String || profile is! String) continue;
        if (!announced.contains(ns) || isBuiltIn(ns)) continue;
        if (!kMMKnownProfiles.contains(profile)) continue;
        final bots = <String>{
          for (final b in (e['bots'] is List ? e['bots'] as List : const []))
            if (b is String && _looksLikeUserId(b)) b,
        };
        extras[ns] = _ExtraNamespace(profile, bots);
      }
    }
    return MMEventNamespaces._(extras, announced);
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
