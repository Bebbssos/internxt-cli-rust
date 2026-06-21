// Reference vectors for the workspace key-decryption crypto (PGP + Kyber512 + blake3).
// Mirrors og/cli keys.service.ts hybridDecryptMessageWithPrivateKey (inverted to encrypt).
// Run from repo root: node scripts/ref-keys.js > scripts/ref-keys.json
// Requires og/ fetched (uses og/node_modules openpgp, @dashlane kyber, hash-wasm, @internxt/lib).
//
// The Rust side (tests/keys_crypto.rs) reads the emitted JSON and must reproduce
// each `expected` plaintext from the given private keys + encrypted blob.
const path = require('node:path');
const Module = require('node:module');
// Resolve deps from og/node_modules regardless of cwd.
Module.globalPaths.push(path.join(__dirname, '..', 'og', 'node_modules'));

const openpgp = require('openpgp');
const kemBuilder = require('@dashlane/pqc-kem-kyber512-node').default;
const { blake3 } = require('hash-wasm');
const aesLib = require('@internxt/lib').aes;

const WORDS_HYBRID_MODE_IN_BASE64 = 'SHlicmlkTW9kZQ=='; // 'HybridMode'

// XOR two equal-length hex strings (mirror CryptoUtils.XORhex).
function XORhex(a, b) {
  if (a.length !== b.length) throw new Error('XORhex length mismatch');
  let res = '';
  for (let i = 0; i < a.length; i++) {
    res += (parseInt(a[i], 16) ^ parseInt(b[i], 16)).toString(16);
  }
  return res;
}

async function eccEncrypt(plaintext, publicKeyArmored) {
  const publicKey = await openpgp.readKey({ armoredKey: publicKeyArmored });
  const message = await openpgp.createMessage({ text: plaintext });
  const armored = await openpgp.encrypt({ message, encryptionKeys: publicKey, format: 'armored' });
  return armored;
}

async function main() {
  const PASSWORD = 'correct horse battery staple';
  const MNEMONIC =
    'truck arch ostrich found cabin matrix dial nothing have orphan teach also ' +
    'kingdom shrug abandon flush draw flat upon arena buffalo ankle erase glow';

  // --- generate an ecc (ed25519Legacy) OpenPGP keypair ---
  const { privateKey: eccPrivArmored, publicKey: eccPubArmored } = await openpgp.generateKey({
    userIDs: [{ email: 'inxt@inxt.com' }],
    curve: 'ed25519Legacy',
    format: 'armored',
  });
  // Login stores ecc private key as base64(armored).
  const eccPrivB64 = Buffer.from(eccPrivArmored).toString('base64');

  // Also produce the AES-GCM (lib) encrypted form to verify decrypt_private_key.
  const eccPrivEncrypted = aesLib.encrypt(eccPrivArmored, PASSWORD);

  // --- generate a kyber512 keypair ---
  const kem = await kemBuilder();
  const { publicKey: kyberPub, privateKey: kyberPriv } = await kem.keypair();
  const kyberPrivB64 = Buffer.from(kyberPriv).toString('base64');

  // === ecc-only vector ===
  const eccOnlyArmored = await eccEncrypt(MNEMONIC, eccPubArmored);
  const eccOnlyBlob = Buffer.from(eccOnlyArmored).toString('base64');

  // === hybrid vector ===
  const { ciphertext: kyberCt, sharedSecret } = await kem.encapsulate(kyberPub);
  const msgHex = Buffer.from(MNEMONIC, 'utf8').toString('hex');
  const bits = msgHex.length * 4;
  const secretHex = await blake3(new Uint8Array(sharedSecret), bits);
  const eccPlaintext = XORhex(msgHex, secretHex); // what the ecc layer carries
  const eccArmoredHybrid = await eccEncrypt(eccPlaintext, eccPubArmored);
  const eccCtB64 = Buffer.from(eccArmoredHybrid).toString('base64');
  const kyberCtB64 = Buffer.from(kyberCt).toString('base64');
  const hybridBlob = [WORDS_HYBRID_MODE_IN_BASE64, kyberCtB64, eccCtB64].join('$');

  // standalone kyber decapsulation vector
  const kyberSharedB64 = Buffer.from(sharedSecret).toString('base64');

  process.stdout.write(
    JSON.stringify(
      {
        password: PASSWORD,
        ecc_private_key_b64: eccPrivB64,
        ecc_private_key_encrypted: eccPrivEncrypted,
        kyber_private_key_b64: kyberPrivB64,
        kyber_ciphertext_b64: kyberCtB64,
        kyber_shared_secret_b64: kyberSharedB64,
        secret_hex_blake3: secretHex,
        ecc_only_blob: eccOnlyBlob,
        hybrid_blob: hybridBlob,
        expected_mnemonic: MNEMONIC,
      },
      null,
      2,
    ) + '\n',
  );
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
