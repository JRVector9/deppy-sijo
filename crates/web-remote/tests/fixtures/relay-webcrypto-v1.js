(() => {
  "use strict";

  const fixture = JSON.parse(document.getElementById("relay-fixture").textContent);
  const encoder = new TextEncoder();

  function bytesFromHex(value) {
    if (
      typeof value !== "string" ||
      value.length % 2 !== 0 ||
      !/^(?:[0-9a-fA-F]{2})*$/.test(value)
    ) {
      throw new Error("invalid fixture hex");
    }
    return Uint8Array.from(value.match(/.{2}/g) ?? [], (byte) => Number.parseInt(byte, 16));
  }

  function hexFromBytes(value) {
    return Array.from(new Uint8Array(value), (byte) => byte.toString(16).padStart(2, "0")).join("");
  }

  function assertEqual(actual, expected, label) {
    if (actual !== expected) {
      throw new Error(`${label}: expected ${expected}, got ${actual}`);
    }
  }

  function concatBytes(...parts) {
    const output = new Uint8Array(parts.reduce((length, part) => length + part.length, 0));
    let offset = 0;
    for (const part of parts) {
      output.set(part, offset);
      offset += part.length;
    }
    return output;
  }

  function fixedBytes(value, byteLength, label) {
    const bytes = bytesFromHex(value);
    if (bytes.length !== byteLength) {
      throw new Error(`${label}: expected ${byteLength} bytes, got ${bytes.length}`);
    }
    return bytes;
  }

  function u32be(value) {
    if (!Number.isSafeInteger(value) || value < 0 || value > 0xffffffff) {
      throw new Error("u32 value is out of range");
    }
    const bytes = new Uint8Array(4);
    new DataView(bytes.buffer).setUint32(0, value, false);
    return bytes;
  }

  function u64be(value) {
    let remaining = BigInt(value);
    if (remaining < 0n || remaining > 0xffffffffffffffffn) {
      throw new Error("u64 value is out of range");
    }
    const bytes = new Uint8Array(8);
    for (let index = bytes.length - 1; index >= 0; index -= 1) {
      bytes[index] = Number(remaining & 0xffn);
      remaining >>= 8n;
    }
    return bytes;
  }

  function roleCode(role) {
    if (role === "desktop") return 1;
    if (role === "device") return 2;
    throw new Error(`unsupported Relay role: ${role}`);
  }

  function directionContract(direction) {
    if (direction === "desktop-to-device") return { code: 1, nonceDomain: "D2DV" };
    if (direction === "device-to-desktop") return { code: 2, nonceDomain: "V2DS" };
    throw new Error(`unsupported Relay direction: ${direction}`);
  }

  function buildTranscript(contract) {
    return concatBytes(
      encoder.encode("deppy-relay-handshake-v1\0"),
      u32be(contract.protocol_version),
      fixedBytes(contract.connection_id_hex, 16, "connection id"),
      Uint8Array.of(roleCode(contract.desktop.role)),
      fixedBytes(contract.desktop.identity_public_sec1_hex, 65, "desktop identity"),
      fixedBytes(contract.desktop.ephemeral_public_sec1_hex, 65, "desktop ephemeral"),
      Uint8Array.of(roleCode(contract.device.role)),
      fixedBytes(contract.device.identity_public_sec1_hex, 65, "device identity"),
      fixedBytes(contract.device.ephemeral_public_sec1_hex, 65, "device ephemeral"),
    );
  }

  function buildEnvelopeNonce(direction, sequence) {
    const contract = directionContract(direction);
    return concatBytes(encoder.encode(contract.nonceDomain), u64be(sequence));
  }

  function buildEnvelopeAad(
    protocolVersion,
    connectionId,
    direction,
    sequence,
    senderFingerprint,
    recipientFingerprint,
    ciphertextLength,
  ) {
    return concatBytes(
      encoder.encode("deppy-relay-envelope-aad-v1\0"),
      u32be(protocolVersion),
      connectionId,
      senderFingerprint,
      recipientFingerprint,
      Uint8Array.of(directionContract(direction).code),
      u64be(sequence),
      u32be(ciphertextLength),
    );
  }

  async function sha256(value) {
    return new Uint8Array(await crypto.subtle.digest("SHA-256", value));
  }

  async function verifySignature(peer, transcript) {
    const key = await crypto.subtle.importKey(
      "raw",
      bytesFromHex(peer.identity_public_sec1_hex),
      { name: "ECDSA", namedCurve: "P-256" },
      false,
      ["verify"],
    );
    const verified = await crypto.subtle.verify(
      { name: "ECDSA", hash: "SHA-256" },
      key,
      bytesFromHex(peer.signature_raw_hex),
      transcript,
    );
    if (!verified) {
      throw new Error(`${peer.role} ECDSA signature did not verify`);
    }
  }

  async function signTranscript(peer, transcript) {
    const privateKey = await crypto.subtle.importKey(
      "jwk",
      peer.identity_private_jwk,
      { name: "ECDSA", namedCurve: "P-256" },
      false,
      ["sign"],
    );
    const signature = new Uint8Array(
      await crypto.subtle.sign({ name: "ECDSA", hash: "SHA-256" }, privateKey, transcript),
    );
    if (signature.length !== 64) {
      throw new Error(`browser ECDSA signature is ${signature.length} bytes, expected raw r||s`);
    }
    return signature;
  }

  async function deriveSharedSecret(privatePeer, publicPeer) {
    const privateKey = await crypto.subtle.importKey(
      "jwk",
      privatePeer.ephemeral_private_jwk,
      { name: "ECDH", namedCurve: "P-256" },
      false,
      ["deriveBits"],
    );
    const publicKey = await crypto.subtle.importKey(
      "raw",
      bytesFromHex(publicPeer.ephemeral_public_sec1_hex),
      { name: "ECDH", namedCurve: "P-256" },
      false,
      [],
    );
    return crypto.subtle.deriveBits({ name: "ECDH", public: publicKey }, privateKey, 256);
  }

  async function deriveHkdf(sharedSecret, salt, info, bitLength) {
    const key = await crypto.subtle.importKey("raw", sharedSecret, "HKDF", false, ["deriveBits"]);
    return crypto.subtle.deriveBits(
      {
        name: "HKDF",
        hash: "SHA-256",
        salt,
        info: encoder.encode(info),
      },
      key,
      bitLength,
    );
  }

  async function verifyEnvelope(vector, keyHex, senderFingerprint, recipientFingerprint) {
    const connectionId = fixedBytes(fixture.connection_id_hex, 16, "connection id");
    const ciphertext = bytesFromHex(vector.ciphertext_and_tag_hex);
    const sequence = BigInt(vector.sequence);
    const nonce = buildEnvelopeNonce(vector.direction, sequence);
    const aad = buildEnvelopeAad(
      fixture.protocol_version,
      connectionId,
      vector.direction,
      sequence,
      senderFingerprint,
      recipientFingerprint,
      ciphertext.length,
    );
    assertEqual(hexFromBytes(nonce), vector.nonce_hex, `${vector.direction} nonce`);
    assertEqual(hexFromBytes(aad), vector.aad_hex, `${vector.direction} AAD`);

    const key = await crypto.subtle.importKey(
      "raw",
      bytesFromHex(keyHex),
      { name: "AES-GCM" },
      false,
      ["encrypt", "decrypt"],
    );
    const algorithm = {
      name: "AES-GCM",
      iv: nonce,
      additionalData: aad,
      tagLength: 128,
    };
    const plaintext = await crypto.subtle.decrypt(
      algorithm,
      key,
      ciphertext,
    );
    assertEqual(hexFromBytes(plaintext), vector.plaintext_hex, `${vector.direction} decrypt`);
    const encrypted = await crypto.subtle.encrypt(
      algorithm,
      key,
      bytesFromHex(vector.plaintext_hex),
    );
    assertEqual(
      hexFromBytes(encrypted),
      vector.ciphertext_and_tag_hex,
      `${vector.direction} encrypt`,
    );
  }

  async function run() {
    if (!globalThis.crypto?.subtle) {
      throw new Error("WebCrypto SubtleCrypto is unavailable");
    }

    const transcript = buildTranscript(fixture);
    assertEqual(hexFromBytes(transcript), fixture.transcript_hex, "handshake transcript");
    const [desktopFingerprint, deviceFingerprint] = await Promise.all([
      sha256(fixedBytes(fixture.desktop.identity_public_sec1_hex, 65, "desktop identity")),
      sha256(fixedBytes(fixture.device.identity_public_sec1_hex, 65, "device identity")),
    ]);
    await Promise.all([
      verifySignature(fixture.desktop, transcript),
      verifySignature(fixture.device, transcript),
    ]);
    const browserSignature = await signTranscript(fixture.device, transcript);

    const [desktopShared, deviceShared] = await Promise.all([
      deriveSharedSecret(fixture.desktop, fixture.device),
      deriveSharedSecret(fixture.device, fixture.desktop),
    ]);
    assertEqual(hexFromBytes(desktopShared), fixture.shared_secret_hex, "desktop ECDH");
    assertEqual(hexFromBytes(deviceShared), fixture.shared_secret_hex, "device ECDH");

    const transcriptSaltInput = new Uint8Array([
      ...encoder.encode("deppy-relay-hkdf-salt-v1\0"),
      ...transcript,
    ]);
    const transcriptSalt = await sha256(transcriptSaltInput);
    assertEqual(hexFromBytes(transcriptSalt), fixture.hkdf_salt_hex, "HKDF salt");

    const desktopToDevice = await deriveHkdf(
      desktopShared,
      transcriptSalt,
      "deppy-relay-desktop-to-device-v1\0",
      256,
    );
    const deviceToDesktop = await deriveHkdf(
      desktopShared,
      transcriptSalt,
      "deppy-relay-device-to-desktop-v1\0",
      256,
    );
    assertEqual(
      hexFromBytes(desktopToDevice),
      fixture.desktop_to_device_key_hex,
      "desktop-to-device HKDF",
    );
    assertEqual(
      hexFromBytes(deviceToDesktop),
      fixture.device_to_desktop_key_hex,
      "device-to-desktop HKDF",
    );

    const sasBytes = new Uint8Array(
      await deriveHkdf(desktopShared, transcriptSalt, "deppy-relay-sas-v1\0", 32),
    );
    const sasNumber =
      (((sasBytes[0] << 24) >>> 0) |
        (sasBytes[1] << 16) |
        (sasBytes[2] << 8) |
        sasBytes[3]) >>>
      0;
    assertEqual(String(sasNumber % 1_000_000).padStart(6, "0"), fixture.sas, "SAS");

    await verifyEnvelope(
      fixture.desktop_to_device,
      fixture.desktop_to_device_key_hex,
      desktopFingerprint,
      deviceFingerprint,
    );
    await verifyEnvelope(
      fixture.device_to_desktop,
      fixture.device_to_desktop_key_hex,
      deviceFingerprint,
      desktopFingerprint,
    );
    document.body.dataset.status = "ok";
    document.body.textContent = `RELAY_WEBCRYPTO_OK:${hexFromBytes(browserSignature)}`;
  }

  run().catch((error) => {
    document.body.dataset.status = "error";
    document.body.textContent = `RELAY_WEBCRYPTO_ERROR:${error?.stack ?? error}`;
  });
})();
