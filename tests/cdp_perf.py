"""CDP-замер производительности UI: куча, long tasks, кадры.

Диагностический скрипт (не gate CI): поднимает сервер с фикстурами,
воспроизводит трек с текстом, скроллит список и сохраняет метрики
Performance.getMetrics в dist/cdp-metrics.json. Ассёртит только отсутствие
ошибок страницы; числа — для отчёта, пороги не фиксируются (headless-раннер
шумный). Для сравнения «до/после» запускать на одном железе.
"""
import json
import os
import socket
import subprocess
import sys
import time
from pathlib import Path
from tempfile import TemporaryDirectory
from urllib.request import urlopen

from playwright.sync_api import expect, sync_playwright

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("PUMP_TEST_BINARY", str(ROOT / "target/debug/pump.exe"))).resolve()
OUT = ROOT / "dist" / "cdp-metrics.json"

WANT = ("JSHeapUsedSize", "JSHeapTotalSize", "Nodes", "JSEventListeners",
        "LayoutCount", "RecalcStyleCount", "LayoutDuration", "RecalcStyleDuration",
        "ScriptDuration", "TaskDuration")


def make_audio(path, frequency, title):
    subprocess.run(
        ["ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error",
         "-f", "lavfi", "-i", f"sine=frequency={frequency}:duration=40",
         "-b:a", "96k", "-metadata", f"title={title}", "-metadata",
         "artist=PUMP Test Artist", str(path)],
        check=True,
    )


def main():
    folder = TemporaryDirectory(prefix="pump-cdp-")
    root = Path(folder.name)
    (root / "music").mkdir()
    make_audio(root / "music" / "alpha.mp3", 440, "North Loop")
    with socket.socket() as chosen:
        chosen.bind(("127.0.0.1", 0))
        port = chosen.getsockname()[1]
    origin = f"http://127.0.0.1:{port}"
    env = {**os.environ, "BIND_ADDR": "127.0.0.1", "PORT": str(port),
           "PUMP_MEDIA_DIR": str(root / "music"), "PUMP_DATA_DIR": str(root / "data")}
    for key in ("TELOXIDE_TOKEN", "TELEGRAM_BOT_TOKEN", "WEBAPP_URL", "YOUTUBE_API_KEY"):
        env.pop(key, None)
    log = (root / "pump.log").open("w+")
    server = subprocess.Popen([str(BINARY)], cwd=root, env=env, stdout=log, stderr=log)
    result = {"isolated": None}
    try:
        for _ in range(100):
            try:
                with urlopen(f"http://127.0.0.1:{port}/healthz", timeout=2) as r:
                    if r.status == 200:
                        break
            except OSError:
                if server.poll() is not None:
                    raise RuntimeError("server did not start")
            time.sleep(0.05)
        lines = "".join(f"[00:{i // 60:02d}.{i % 60:02d}] Line {i} of the song here\n" for i in range(1, 301))
        with sync_playwright() as driver:
            browser = driver.chromium.launch()
            page = browser.new_page(viewport={"width": 390, "height": 844})
            errors = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.route("**/api/lyrics/**", lambda route: route.fulfill(
                json={"synced": lines, "plain": None}))
            page.goto(origin)
            cdp = page.context.new_cdp_session(page)
            cdp.send("Performance.enable", {"timeDomain": "timeTicks"})
            result["isolated"] = page.evaluate("crossOriginIsolated")
            metrics = cdp.send("Performance.getMetrics")["metrics"]
            result["baseline"] = {m["name"]: m["value"] for m in metrics if m["name"] in WANT}
            page.evaluate("""() => new Promise(resolve => {
                window.__longtasks = [];
                const obs = new PerformanceObserver(list => { for (const e of list.getEntries()) window.__longtasks.push(e.duration); });
                obs.observe({entryTypes: ["longtask"]});
                setTimeout(resolve, 100);
            })""")
            page.locator("#tabSearch").click()
            page.locator("#source").select_option("full")
            page.locator("#qInput").fill("North")
            page.get_by_role("button", name="North Loop", exact=False).click()
            page.locator("#tabLyrics").click()
            page.locator("#audio").evaluate("(a) => { a.currentTime = 2.0; }")
            page.wait_for_timeout(3000)
            longtasks = page.evaluate("window.__longtasks")
            result["longtasks_during_lyrics_ms"] = longtasks
            metrics = cdp.send("Performance.getMetrics")["metrics"]
            result["after"] = {m["name"]: m["value"] for m in metrics if m["name"] in WANT}
            result["pageerrors"] = errors
            assert errors == [], errors
            browser.close()
        OUT.parent.mkdir(parents=True, exist_ok=True)
        OUT.write_text(json.dumps(result, ensure_ascii=False, indent=1), encoding="utf-8")
        print(f"saved {OUT} longtasks={len(longtasks)}")
        return 0
    finally:
        if server.poll() is None:
            server.terminate()
            server.wait(timeout=5)
        log.close()
        folder.cleanup()


if __name__ == "__main__":
    sys.exit(main())
