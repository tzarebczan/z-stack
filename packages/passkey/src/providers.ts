/**
 * Friendly names for common passkey providers, keyed by AAGUID. For display
 * only: AAGUIDs are not attested for synced passkeys and can be spoofed, so
 * never make a security decision from them. The community registry
 * (github.com/passkeydeveloper/passkey-authenticator-aaguids) has the full
 * list with icons if you want more.
 */
const PROVIDERS: Record<string, string> = {
  "ea9b8d66-4d01-1d21-3ce4-b6b48cb575d4": "Google Password Manager",
  "adce0002-35bc-c60a-648b-0b25f1f05503": "Chrome on Mac",
  "fbfc3007-154e-4ecc-8c0b-6e020557d7bd": "iCloud Keychain",
  "dd4ec289-e01d-41c9-bb89-70fa845d4bf2": "iCloud Keychain (Managed)",
  "08987058-cadc-4b81-b6e1-30de50dcbe96": "Windows Hello",
  "9ddd1817-af5a-4672-a2b9-3e3dd95000a9": "Windows Hello",
  "6028b017-b1d4-4c02-b4b3-afcdafc96bb2": "Windows Hello",
  "bada5566-a7aa-401f-bd96-45619a55120d": "1Password",
  "d548826e-79b4-db40-a3d8-11116f7e8349": "Bitwarden",
  "531126d6-e717-415c-9320-3d9aa6981239": "Dashlane",
  "fdb141b2-5d84-443e-8a35-4698c205a502": "KeePassXC",
  "50726f74-6f6e-5061-7373-50726f746f6e": "Proton Pass",
  "53414d53-554e-4700-0000-000000000000": "Samsung Pass",
};

/** Provider name for an AAGUID, if it is a well-known one. */
export function passkeyProviderName(aaguid: string | undefined): string | undefined {
  return aaguid ? PROVIDERS[aaguid.toLowerCase()] : undefined;
}
