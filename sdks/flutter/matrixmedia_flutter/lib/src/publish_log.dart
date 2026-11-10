/// One-line publish diagnostics for the go-live path.
///
/// Pure (no `dart:html`) so unit tests can pin the format. The web publisher
/// logs this right after capture and right after `createOffer`, so a single
/// browser-console paste distinguishes "page got no devices" (zero tracks)
/// from "offer carried no media" (tracks present, no sendonly m-lines).
String describePublishCapture({
  required List<String> trackKinds,
  required String offerSdp,
}) {
  final mLines = RegExp(r'^m=(audio|video)', multiLine: true)
      .allMatches(offerSdp)
      .map((m) => m.group(1))
      .join(',');
  final senders = RegExp(r'a=(sendonly|sendrecv)', multiLine: true)
      .allMatches(offerSdp)
      .length;
  return 'capture tracks=${trackKinds.length}'
      ' [${trackKinds.join(',')}]'
      ' offer m-lines=[$mLines] senders=$senders';
}
