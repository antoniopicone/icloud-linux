import sys, hashlib, base64
sys.path.insert(0, "/private/tmp/claude-501/-Users-antonio-Developer-icloud-linux/f8c60f02-e859-4a56-871e-cfcadc887030/scratchpad/ref/srp-1.0.22")
import srp._pysrp as srp
srp.rfc5054_enable(); srp.no_username_in_x()

def derive(password, salt, iters, proto):
    h = hashlib.sha256(password.encode()).digest()
    if proto == "s2k_fo": h = h.hex().encode()
    return hashlib.pbkdf2_hmac("sha256", h, salt, iters, 32)

salt = bytes(range(16))
for proto in ("s2k", "s2k_fo"):
    print("DERIVE", proto, derive("correct horse battery staple", salt, 20000, proto).hex())

a = bytes([0x80 | 0x11]) + bytes((i * 7 + 3) % 256 for i in range(255))
# Server public value: any residue works for the client side; use a real one.
N, g = srp._pysrp.get_ng(srp.NG_2048, None, None) if hasattr(srp, '_pysrp') else srp.get_ng(srp.NG_2048, None, None)
b_pub = pow(g, int.from_bytes(bytes((i * 13 + 5) % 256 for i in range(256)), "big"), N)
B = b_pub.to_bytes((b_pub.bit_length() + 7) // 8, "big")
pw = derive("correct horse battery staple", salt, 20000, "s2k_fo")
usr = srp.User("user@example.com", pw, hash_alg=srp.SHA256, ng_type=srp.NG_2048, bytes_a=a)
uname, A = usr.start_authentication()
M = usr.process_challenge(salt, B)
print("A", A.hex()); print("B", B.hex()); print("M1", M.hex()); print("M2", usr.H_AMK.hex())
print("SECRET", a.hex())
