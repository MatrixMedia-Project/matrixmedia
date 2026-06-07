# Acceptable Use & Reporting Policy

> **DRAFT — pending legal review.** This document describes how MatrixMedia
> handles abuse reports and enforcement today. It is an engineering draft and
> has not yet been reviewed by counsel; the final published policy may differ.

MatrixMedia is an open, federated media platform. To keep it usable and lawful,
everyone who streams, posts, donates, or chats agrees to the rules below.

## What's not allowed

- **Illegal content** — anything unlawful where the operator or the user is
  located, including content that sexually exploits or endangers minors (CSAM),
  which is reported to the appropriate authorities and never tolerated.
- **Harassment & threats** — targeted abuse, stalking, doxxing, or incitement
  of violence against a person or group.
- **Hateful conduct** — content that promotes violence or hatred against people
  based on protected characteristics.
- **Fraud & deception** — scams, payment fraud, impersonation, or misleading
  donation/subscription solicitations.
- **Malware & platform abuse** — distributing malicious software, spamming, or
  attempting to disrupt or overload the service.
- **Non-consensual intimate media** and other content that violates a person's
  privacy or dignity.

Creators are responsible for the content they broadcast and for content in the
channels they host.

## How to report

- **In the apps:** use the **Report** action on a message, user, room, live
  stream, or recording. Reports go to the operator's moderation queue.
- **By email:** contact <a href="mailto:argi@steegler.com">argi@steegler.com</a>
  with a link or identifier (channel, stream, recording, or user ID) and a short
  description.

Reports are reviewed by a human operator. Please report in good faith — repeated
bad-faith or automated reporting may itself be treated as abuse.

## What happens after a report

1. The report enters the operator moderation queue.
2. An operator reviews the reported content and context.
3. The operator takes an action proportionate to the violation (see below), or
   dismisses the report if no violation is found.
4. Every action is recorded in an append-only audit log with the operator,
   reason, and timestamp.

## Enforcement actions

Operators can take graduated action depending on severity:

- **Content removal** — force-stop a live stream, or hide / delete a recording.
- **Account suspension** *(reversible)* — a suspended account can still sign in
  and read, but cannot stream, broadcast, or transact (donations/subscriptions)
  until the suspension is lifted.
- **Account deactivation** *(permanent)* — for severe or repeated violations,
  the Matrix account is deactivated.

## Appeals

If you believe an action against your content or account was a mistake, you can
appeal by emailing <a href="mailto:argi@steegler.com">argi@steegler.com</a> with
your Matrix user ID and the action you're appealing. We aim to respond within a
reasonable time. Suspensions are reversible; if an appeal succeeds the
restriction is lifted.

## Source

MatrixMedia is open-source under Apache-2.0. The moderation pipeline that
enforces this policy is in
<a href="https://github.com/MatrixMedia-Project/matrixmedia">github.com/MatrixMedia-Project/matrixmedia</a>.
