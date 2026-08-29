# SAML 2.0 Enterprise SSO

This document describes the SAML 2.0 service provider implemented in
`backend/src/saml.rs`. It adds enterprise single sign-on on top of the
existing basic credential flow, so customers can authenticate against their
own identity provider (Okta, Entra ID, Ping, Google Workspace).

## Why

With only basic auth, every enterprise user needs a separate credential in
this backend, outside the customer's own identity lifecycle. Deprovisioning a
departing employee in the corporate IdP would not revoke their access here.
Implementing the SAML web browser SSO profile puts authentication back where
the customer already manages it.

## Flow

```text
GET  /saml/metadata → get_metadata                (XML to paste into the IdP)
GET  /saml/login    → initiate_login              (AuthnRequest + request ID)
POST /saml/acs      → assertion_consumer_service
                        base64-decode SAMLResponse
                        parse_response      → SamlAssertion
                        validate_assertion  → issuer, audience, window,
                                              InResponseTo, replay, signer
                        map_attributes      → SamlUser → session
```

Both SP-initiated and IdP-initiated (unsolicited) SSO are supported. An
unsolicited assertion carries no `InResponseTo`, so that check is skipped
while every other check still applies.

## Configuration

`SamlConfig` holds one IdP integration:

| Field | Meaning |
| --- | --- |
| `entity_id` | SP entity ID, and the audience assertions must be scoped to |
| `acs_url` | Absolute URL of the ACS endpoint, advertised in metadata |
| `idp_entity_id` | Expected assertion issuer |
| `idp_sso_url` | Where `AuthnRequest`s are POSTed |
| `idp_certificate` | Base64 body of the IdP signing certificate, no PEM armor |
| `clock_skew_secs` | Tolerance applied to `NotBefore` / `NotOnOrAfter` |

`certificate_fingerprint` returns the SHA-256 hex digest of a certificate
body, for pinning it in config review and audit logs.

## Assertion validation

`validate_assertion` refuses anything that fails these checks, each with its
own `SamlError` variant:

- **Status** - the response `StatusCode` must end in `:Success`
  (`StatusNotSuccess`), checked during parsing.
- **Issuer** - must equal `idp_entity_id` (`IssuerMismatch`).
- **Audience** - `AudienceRestriction` must name `entity_id`
  (`AudienceMismatch`).
- **Validity window** - `NotBefore` / `NotOnOrAfter` from both `Conditions`
  and `SubjectConfirmationData`, using whichever expiry is tighter, widened
  by `clock_skew_secs` (`OutsideValidityWindow`).
- **Signer** - a `<Signature>` must be present (`Unsigned`) and its
  `<X509Certificate>` must match the pinned certificate, whitespace ignored
  (`UntrustedSigner`).
- **Request correlation** - a present `InResponseTo` must match an
  `AuthnRequest` this SP issued and has not consumed (`UnknownRequest`).
- **Replay** - each assertion ID may be consumed once (`Replayed`).

Pending request IDs and consumed assertion IDs live in `SamlSecurityState`,
which is shared across requests through `SamlState`.

### Signature verification scope

The signature check pins the signing certificate; it does not perform
XML-DSig RSA digest verification, which requires an XML security library.
Deployments must therefore terminate `/saml/acs` over TLS and keep
`idp_certificate` pinned to the IdP's current signing certificate, rotating
it when the IdP rotates.

## Attribute mapping

`AttributeMapping` names the IdP claims to read. The defaults are the
standard WS-Federation claim URIs used by Okta and Entra ID:

```json
{
  "email": "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/emailaddress",
  "display_name": "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/displayname",
  "groups": "http://schemas.xmlsoap.org/claims/Group"
}
```

For an IdP that sends LDAP-style names, override them:

```json
{ "email": "mail", "display_name": "cn", "groups": "memberOf" }
```

`map_attributes` produces a `SamlUser` with `subject`, `email`,
`display_name`, `groups`, and `session_index`. Multi-valued attributes such
as group membership keep every value. When the IdP omits the email claim, the
`NameID` is used if it looks like an address; otherwise mapping fails with
`MissingAttribute`.

## ACS endpoint

`POST /saml/acs` accepts the HTTP POST binding form fields `SAMLResponse`
(base64) and optional `RelayState`. On success it returns `200` with the
mapped user and the relay state:

```json
{
  "user": {
    "subject": "ada@corp.example",
    "email": "ada@corp.example",
    "display_name": "Ada Lovelace",
    "groups": ["engineering", "vault-admins"],
    "session_index": "idx-99"
  },
  "relay_state": "/dashboard"
}
```

Any rejected assertion returns `401` with the specific reason, and is logged
at warn level so failed SSO attempts are visible in operations tooling:

```json
{ "error": "assertion was signed by an untrusted certificate" }
```

`relay_state` is the deep link the browser should be sent back to after the
session is established.

## Setup

1. Serve `GET /saml/metadata` and register the resulting XML with the IdP, or
   configure the IdP manually with the SP entity ID and ACS URL it contains.
2. Copy the IdP entity ID, SSO URL, and signing certificate into
   `SamlConfig`. Strip the `-----BEGIN CERTIFICATE-----` armor and keep only
   the base64 body.
3. Set `clock_skew_secs` to match the fleet's clock discipline. 60 seconds is
   a reasonable default for NTP-synced hosts.
4. Configure the IdP to sign assertions and to send email, display name, and
   group claims. Adjust `AttributeMapping` if the claim names differ.
5. Verify end to end: hit `GET /saml/login`, POST the returned
   `saml_request` to the IdP, and confirm the ACS response contains the
   expected email and groups.

## Limitations

- Single Logout (SLO) is not implemented; sessions expire locally.
- Encrypted assertions (`EncryptedAssertion`) are not supported, so the IdP
  must be configured to sign without encrypting.
- One IdP per configuration. Multi-tenant SSO needs one `SamlState` per
  tenant, selected before the ACS handler runs.
