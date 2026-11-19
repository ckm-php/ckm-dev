<?php
// The smoke test of a relocated PHP build. Run it with every shared
// extension loaded and the relocation contract's settings (build/php/smoke.sh,
// T4, T7), with no network. It exits 0 and prints "smoke: ok" when every
// check passes; otherwise it prints each failure to stderr and exits 1.
//
// Optional environment:
//   CK_PHP_EXPECT_VERSION  the exact PHP_VERSION, such as 8.5.12
//   CK_PHP_EXPECT_BUILTIN  space-separated extensions that must be built in
//
// PHP 7.4 syntax only.

declare(strict_types=1);

$failures = [];
$passed = 0;

function check(string $name, callable $test): void
{
    global $failures, $passed;
    try {
        $result = $test();
    } catch (Throwable $error) {
        $result = get_class($error) . ': ' . $error->getMessage();
    }
    if ($result === true) {
        $passed++;
    } else {
        $failures[] = $name . ': ' . (is_string($result) ? $result : 'failed');
    }
}

function need(string $extension): void
{
    if (!extension_loaded($extension)) {
        throw new RuntimeException("the extension $extension is not loaded");
    }
}

function same($want, $got): bool
{
    if ($want !== $got) {
        throw new RuntimeException('expected ' . var_export($want, true) . ', got ' . var_export($got, true));
    }
    return true;
}

function ini_file(string $setting): string
{
    $path = (string) ini_get($setting);
    if ($path === '' || !is_file($path)) {
        throw new RuntimeException("$setting is not set to a file ('$path')");
    }
    return $path;
}

$expectVersion = (string) getenv('CK_PHP_EXPECT_VERSION');
$expectBuiltin = trim((string) getenv('CK_PHP_EXPECT_BUILTIN'));

check('version', function () use ($expectVersion) {
    if ($expectVersion !== '') {
        same($expectVersion, PHP_VERSION);
    }
    same(8, PHP_INT_SIZE);
    // NTS only.
    return same(false, (bool) ZEND_THREAD_SAFE);
});

check('builtin', function () use ($expectBuiltin) {
    $missing = [];
    foreach (preg_split('/\s+/', $expectBuiltin, -1, PREG_SPLIT_NO_EMPTY) as $name) {
        // opcache registers as "Zend OPcache".
        $module = $name === 'opcache' ? 'Zend OPcache' : $name;
        if (!extension_loaded($module)) {
            $missing[] = $name;
        }
    }
    return $missing === [] ? true : 'not built in: ' . implode(' ', $missing);
});

check('openssl', function () {
    need('openssl');
    // The stack's OpenSSL: 1.1.1 for 7.4 and 8.0, 3 from 8.1; never the
    // system's LibreSSL.
    $series = PHP_VERSION_ID >= 80100 ? 'OpenSSL 3.' : 'OpenSSL 1.1.1';
    if (strpos(OPENSSL_VERSION_TEXT, $series) !== 0) {
        return 'OPENSSL_VERSION_TEXT is ' . OPENSSL_VERSION_TEXT . ", expected $series";
    }
    $strong = false;
    $bytes = openssl_random_pseudo_bytes(32, $strong);
    same(32, strlen($bytes));
    same(true, $strong);
    $sha = 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad';
    same($sha, openssl_digest('abc', 'sha256'));
    same($sha, hash('sha256', 'abc'));
    $bundle = (string) file_get_contents(ini_file('openssl.cafile'));
    $start = strpos($bundle, '-----BEGIN CERTIFICATE-----');
    $end = strpos($bundle, '-----END CERTIFICATE-----');
    if ($start === false || $end === false) {
        return 'openssl.cafile holds no certificate';
    }
    $pem = substr($bundle, $start, $end - $start + strlen('-----END CERTIFICATE-----'));
    $parsed = openssl_x509_parse($pem);
    return is_array($parsed) && isset($parsed['subject']) ? true : 'cannot parse the first CA certificate';
});

check('curl', function () {
    need('curl');
    $info = curl_version();
    $ssl = (string) ($info['ssl_version'] ?? '');
    if (strpos($ssl, 'OpenSSL/') !== 0) {
        return "libcurl uses '$ssl', expected the stack's OpenSSL";
    }
    // The OpenSSL libcurl loads at run time is the one PHP was built with.
    $built = PHP_VERSION_ID >= 80100 ? 'OpenSSL/3.' : 'OpenSSL/1.1.1';
    if (strpos($ssl, $built) !== 0) {
        return "libcurl uses $ssl, expected $built";
    }
    if (!in_array('https', $info['protocols'], true)) {
        return 'libcurl has no https';
    }
    ini_file('curl.cainfo');
    $handle = curl_init();
    return $handle !== false ? true : 'curl_init failed';
});

check('intl', function () {
    need('intl');
    same("\u{e9}", Normalizer::normalize("e\u{301}", Normalizer::FORM_C));
    $format = new NumberFormatter('de_DE', NumberFormatter::DECIMAL);
    return same('1.234.567,891', $format->format(1234567.891));
});

check('pdo_sqlite', function () {
    need('pdo_sqlite');
    $db = new PDO('sqlite::memory:', null, null, [PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION]);
    $db->exec('CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)');
    $insert = $db->prepare('INSERT INTO t (name) VALUES (?)');
    $insert->execute(['ck']);
    return same('ck', $db->query('SELECT name FROM t WHERE id = 1')->fetchColumn());
});

check('sqlite3', function () {
    need('sqlite3');
    $db = new SQLite3(':memory:');
    $db->enableExceptions(true);
    $version = (string) $db->querySingle('SELECT sqlite_version()');
    if (strpos($version, '3.') !== 0) {
        return "sqlite_version() is '$version'";
    }
    // The stack's SQLite is built with FTS5.
    $db->exec('CREATE VIRTUAL TABLE f USING fts5(body)');
    $db->exec("INSERT INTO f (body) VALUES ('relocated php build')");
    return same(1, $db->querySingle("SELECT count(*) FROM f WHERE f MATCH 'php'"));
});

check('sodium', function () {
    need('sodium');
    $pair = sodium_crypto_box_keypair();
    same(SODIUM_CRYPTO_BOX_KEYPAIRBYTES, strlen($pair));
    $nonce = random_bytes(SODIUM_CRYPTO_BOX_NONCEBYTES);
    $box = sodium_crypto_box('ck', $nonce, $pair);
    return same('ck', sodium_crypto_box_open($box, $nonce, $pair));
});

check('mbstring', function () {
    need('mbstring');
    same("\u{c4}", mb_strtoupper("\u{e4}", 'UTF-8'));
    // mb_ereg exists only with Oniguruma.
    if (!function_exists('mb_ereg')) {
        return 'mbstring was built without Oniguruma';
    }
    $match = [];
    same(true, (bool) mb_ereg('a(b+)', 'xabbc', $match));
    return same('bb', $match[1]);
});

check('pcre', function () {
    if (!PCRE_JIT_SUPPORT) {
        return 'PCRE was built without JIT support';
    }
    if ((string) ini_get('pcre.jit') !== '1') {
        return 'pcre.jit is off';
    }
    $match = [];
    same(1, preg_match('/(\d+)-(\d+)/', 'a 12-34', $match));
    same('34', $match[2]);
    // A failed JIT allocation shows up here rather than as a wrong match.
    return same(PREG_NO_ERROR, preg_last_error());
});

check('json', function () {
    $text = json_encode(['a' => 1, 'b' => [true, null, 'é']], JSON_UNESCAPED_UNICODE);
    same('{"a":1,"b":[true,null,"é"]}', $text);
    return same(['a' => 1, 'b' => [true, null, 'é']], json_decode($text, true));
});

check('date', function () {
    same('UTC', date_default_timezone_get());
    same('1970-01-01T00:00:00', gmdate('Y-m-d\TH:i:s', 0));
    $date = new DateTime('2020-02-29 12:00', new DateTimeZone('UTC'));
    same('2021-03-01', $date->modify('+1 year')->format('Y-m-d'));
    // The bundled time zone database.
    $berlin = new DateTime('2021-07-01 12:00', new DateTimeZone('Europe/Berlin'));
    return same('+02:00', $berlin->format('P'));
});

check('zlib', function () {
    return same('ck dev', gzdecode(gzencode('ck dev')));
});

check('iconv', function () {
    // UTF-16 is built into glibc; most other charsets are gconv modules,
    // which a bare root does not have.
    return same("\x00\xE9", iconv('UTF-8', 'UTF-16BE', "\u{e9}"));
});

check('xml', function () {
    $doc = new DOMDocument();
    same(true, $doc->loadXML('<a><b>ck</b></a>'));
    same('ck', $doc->getElementsByTagName('b')->item(0)->textContent);
    return same('ck', (string) simplexml_load_string('<a><b>ck</b></a>')->b);
});

check('fileinfo', function () {
    need('fileinfo');
    $info = new finfo(FILEINFO_MIME_TYPE);
    return same('application/pdf', $info->buffer("%PDF-1.4\n"));
});

check('loaded', function () {
    $missing = [];
    foreach (['exif', 'mysqli', 'pdo_mysql', 'Zend OPcache'] as $name) {
        if (!extension_loaded($name)) {
            $missing[] = $name;
        }
    }
    return $missing === [] ? true : 'not loaded: ' . implode(', ', $missing);
});

if ($failures !== []) {
    foreach ($failures as $failure) {
        fwrite(STDERR, "smoke: FAILED $failure\n");
    }
    exit(1);
}
echo "smoke: ok ($passed checks, PHP " . PHP_VERSION . ")\n";
