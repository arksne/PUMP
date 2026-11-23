"""Проверяет плеер по HTTP в двух независимых мобильных сессиях Chromium."""

import json
import os
import re
from pathlib import Path
import shutil
import socket
import subprocess
from tempfile import TemporaryDirectory
import time
import unittest
from urllib.request import Request, urlopen
from uuid import uuid4

from playwright.sync_api import expect, sync_playwright


ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("PUMP_TEST_BINARY", ROOT / "target/debug/pump")).resolve()


def make_audio(path, frequency, title):
    subprocess.run(
        [
            "ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error",
            "-f", "lavfi", "-i", f"sine=frequency={frequency}:duration=40",
            "-b:a", "96k", "-metadata", f"title={title}", "-metadata",
            "artist=PUMP Test Artist", str(path),
        ],
        check=True,
    )


def wait_for_audio(page, predicate):
    deadline = time.monotonic() + 8
    audio = page.locator("#audio")
    while time.monotonic() < deadline:
        if audio.evaluate(predicate):
            return
        page.wait_for_timeout(100)
    state = audio.evaluate("(audio) => ({position: audio.currentTime, paused: audio.paused})")
    raise AssertionError(f"Плеер не перешёл в ожидаемое состояние: {predicate}, {state}")


class PlayerTest(unittest.TestCase):
    def setUp(self):
        self.folder = TemporaryDirectory(prefix="pump-browser-")
        self.addCleanup(self.folder.cleanup)
        self.root = Path(self.folder.name)
        music = self.root / "music"
        music.mkdir()
        make_audio(music / "alpha.mp3", 440, "North Loop")
        make_audio(music / "beta.mp3", 550, "South Loop")
        with socket.socket() as chosen:
            chosen.bind(("127.0.0.1", 0))
            port = chosen.getsockname()[1]
        self.origin = f"http://127.0.0.1:{port}"
        self.environment = {
            **os.environ, "BIND_ADDR": "127.0.0.1", "PORT": str(port),
            "PUMP_MEDIA_DIR": str(music), "PUMP_DATA_DIR": str(self.root / "data"),
        }
        for setting in ("TELOXIDE_TOKEN", "TELEGRAM_BOT_TOKEN", "WEBAPP_URL", "YOUTUBE_API_KEY"):
            self.environment.pop(setting, None)
        log = (self.root / "pump.log").open("w+")
        self.addCleanup(log.close)
        self.log = log
        self.start()
        self.addCleanup(self.stop)

    def start(self):
        self.server = subprocess.Popen(
            [str(BINARY)], cwd=self.root, env=self.environment, stdout=self.log, stderr=self.log
        )
        for _ in range(100):
            try:
                status, _, _ = self.request("/healthz")
                if status == 200:
                    return
            except OSError:
                if self.server.poll() is not None:
                    break
            time.sleep(0.05)
        self.stop()
        self.log.seek(0)
        self.fail(f"Сервер не запустился. {self.log.read()}")

    def stop(self):
        if self.server.poll() is None:
            self.server.terminate()
            try:
                self.server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.server.kill()
                self.server.wait(timeout=5)

    def request(self, path, *, body=None, headers=None):
        headers = dict(headers or {})
        if body is not None:
            headers["Content-Type"] = "application/json"
            body = json.dumps(body).encode()
        with urlopen(Request(self.origin + path, data=body, headers=headers), timeout=12) as response:
            return response.status, response.headers, response.read()

    def test_catalog_and_persistent_statistics(self):
        _, _, data = self.request("/api/search?source=full&q=North")
        [track] = json.loads(data)["tracks"]
        self.assertEqual(track["kind"], "full")
        self.assertEqual(track["artist"], "PUMP Test Artist")
        self.assertGreater(track["duration_ms"], 39_000)
        status, headers, data = self.request(track["audio_url"], headers={"Range": "bytes=100-149"})
        self.assertEqual(status, 206)
        self.assertEqual(headers.get_content_type(), "audio/mpeg")
        self.assertEqual(len(data), 50)

        _, _, data = self.request("/api/search?source=youtube&q=https%3A%2F%2Fyoutu.be%2F5NV6Rdv1a3I")
        [video] = json.loads(data)["tracks"]
        self.assertEqual((video["kind"], video["video_id"], video["audio_url"]), ("youtube", "5NV6Rdv1a3I", None))

        _, _, session = self.request("/api/session", body={})
        auth = {"Authorization": "Bearer " + json.loads(session)["token"]}
        event = {"track_id": track["id"], "event_id": str(uuid4()), "milliseconds": 1200}
        for _ in range(2):
            status, _, _ = self.request("/api/listens", headers=auth, body=event)
            self.assertEqual(status, 204)
        self.stop()
        self.start()
        _, _, data = self.request("/api/stats?period=month", headers=auth)
        stats = json.loads(data)
        self.assertEqual((stats["tracks"], stats["milliseconds"]), (1, 1200))
        self.assertEqual(stats["top_artists"][0]["title"], track["artist"])

    def test_settings_profile_and_offline_audio(self):
        with sync_playwright() as driver:
            browser=driver.chromium.launch(executable_path=os.environ.get("PUMP_TEST_CHROMIUM") or shutil.which("chromium"))
            page=browser.new_page(**driver.devices["Pixel 7"])
            errors=[]
            page.on("pageerror",lambda error:errors.append(str(error)))
            page.route("**/api/lyrics/**",lambda route:route.fulfill(json={"synced":None,"plain":None}))
            page.goto(self.origin)
            self.assertTrue(page.evaluate("() => PumpUX.wasmReady"))
            page.locator("#tabSearch").click()
            page.locator("#source").select_option("full")
            page.locator("#qInput").fill("North")
            page.get_by_role("button",name="North Loop",exact=False).click()
            wait_for_audio(page,"audio => audio.currentTime > 0.4")
            page.locator('[data-action="settings-open"]').click()
            page.locator("#eqEnabled").check()
            page.locator("#autoEqFile").set_input_files({"name":"headphones.txt","mimeType":"text/plain","buffer":b"Preamp: -4 dB\nFilter 1: ON PK Fc 1000 Hz Gain 3 dB Q 1.4"})
            expect(page.locator("#settingsStatus")).to_have_text("Параметрический профиль применён.")
            page.get_by_text("Оформление",exact=True).click()
            page.locator("#designSelect").select_option("classic-glass")
            expect(page.locator("#artBox")).to_have_class(re.compile(r".*\bclassic-glass\b.*"))
            page.get_by_text("Хранилище",exact=True).click()
            page.locator("#downloadButton").click()
            expect(page.locator("#offlineStatus")).to_have_text("Запись доступна на этом устройстве.",timeout=15000)
            expect(page.locator("#downloadsList")).to_contain_text("North Loop")
            self.assertEqual(errors,[])
            browser.close()

    def test_two_people_listen_together(self):
        with sync_playwright() as driver:
            executable = os.environ.get("PUMP_TEST_CHROMIUM") or shutil.which("chromium")
            browser = driver.chromium.launch(executable_path=executable)
            device = driver.devices["Pixel 7"]
            host_context = browser.new_context(**device)
            guest_context = browser.new_context(**device)
            host, guest = host_context.new_page(), guest_context.new_page()
            errors = []
            for page in (host, guest):
                page.on("pageerror", lambda error: errors.append(str(error)))
                page.route("**/api/lyrics/**", lambda route: route.fulfill(json={"synced": None, "plain": None}))
                page.goto(self.origin)
                self.assertTrue(page.locator("#audio").count())

            host.locator("#tabSearch").click()
            host.locator("#source").select_option("full")
            host.locator("#qInput").fill("North")
            host.get_by_role("button", name="North Loop", exact=False).click()
            expect(host.locator("#tTitle")).to_have_text("North Loop")
            wait_for_audio(host, "audio => audio.currentTime > 0.6")
            host.locator("#tabSearch").click()
            host.locator("#qInput").fill("South")
            expect(host.get_by_role("button", name="South Loop", exact=False)).to_be_visible()
            host.locator("#tabPlayer").click()
            expect(host.locator("#tTitle")).to_have_text("North Loop")
            host.locator("#queueButton").click()
            expect(host.locator("#queueList [aria-current]")).to_contain_text("North Loop")
            host.get_by_role("button", name="Закрыть очередь").click()

            host.locator("#tabRooms").click()
            host.locator("#createRoom").click()
            expect(host.locator("#roomStatus")).to_contain_text("Ведущий")
            code = host.locator("#roomCode").input_value()
            self.assertRegex(code, r"^[0-9a-f]{12}$")
            guest.locator("#tabRooms").click()
            guest.locator("#roomCode").fill(code)
            guest.get_by_role("button", name="Войти", exact=True).click()
            expect(guest.locator("#roomStatus")).to_contain_text("Слушатель")
            guest.locator("#enableRoomSound").click()
            guest.locator("#tabPlayer").click()
            expect(guest.locator("#playBtn")).to_be_disabled()
            expect(guest.locator("#tTitle")).to_have_text("North Loop")
            wait_for_audio(guest, "audio => !audio.paused")

            host.locator("#tabPlayer").click()
            host.locator("#seekBar").evaluate(
                "(bar) => { bar.value = 50; bar.dispatchEvent(new Event('input', {bubbles: true})); }"
            )
            wait_for_audio(guest, "audio => audio.currentTime >= 19.8")
            host.locator("#playBtn").click()
            wait_for_audio(guest, "audio => audio.paused")
            host_position = host.locator("#audio").evaluate("(audio) => audio.currentTime")
            guest_position = guest.locator("#audio").evaluate("(audio) => audio.currentTime")
            self.assertLess(abs(host_position - guest_position), 0.2)

            host.locator("#tabStats").click()
            expect(host.locator("#statsTracks")).to_contain_text("North Loop")
            host.locator("#statsPeriod").select_option("month")
            expect(host.locator("#statsArtists")).to_contain_text("PUMP Test Artist")
            self.assertEqual(errors, [])
            browser.close()


if __name__ == "__main__":
    unittest.main(verbosity=2)
