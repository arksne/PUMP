"""Проверяет плеер по HTTP в двух независимых мобильных сессиях Chromium."""

import json
import hashlib
import sqlite3
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
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
from urllib.error import HTTPError
from provider_fixture import module as provider_module
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

    def test_nas_ticket_confines_access_and_streams_ranges(self):
        _,_,session=self.request("/api/session",body={})
        token=json.loads(session)["token"]
        auth={"Authorization":"Bearer "+token}
        with sqlite3.connect(self.root/"data/pump.sqlite") as db:
            owner=db.execute("SELECT owner FROM sessions WHERE token_hash=?",(hashlib.sha256(token.encode()).hexdigest(),)).fetchone()[0]
        media=(self.root/"music/alpha.mp3").read_bytes()
        received=[]
        class WebDavHandler(BaseHTTPRequestHandler):
            def do_GET(handler):
                received.append((handler.path,handler.headers.get("Range")))
                handler.send_response(206)
                handler.send_header("Content-Type","audio/mpeg")
                handler.send_header("Content-Length","100")
                handler.send_header("Content-Range",f"bytes 0-99/{len(media)}")
                handler.end_headers()
                handler.wfile.write(media[:100])
            def log_message(self,*args):
                pass
        nas=ThreadingHTTPServer(("127.0.0.1",0),WebDavHandler)
        thread=threading.Thread(target=nas.serve_forever,daemon=True)
        thread.start()
        self.addCleanup(nas.server_close)
        self.addCleanup(nas.shutdown)
        config=self.root/"nas.json"
        config.write_text(json.dumps([{"protocol":"webdav","id":"home","name":"Home NAS","base":f"http://127.0.0.1:{nas.server_port}/music/","owners":[owner]}]))
        self.stop()
        self.environment["PUMP_NAS_CONFIG"]=str(config)
        self.start()
        _,_,data=self.request("/api/nas",headers=auth)
        self.assertEqual(json.loads(data),[{"id":"home","name":"Home NAS"}])
        _,_,data=self.request("/api/nas/home/ticket",headers=auth,body={"path":"alpha.mp3"})
        ticket=json.loads(data)
        status,_,data=self.request(ticket["url"],headers={"Range":"bytes=0-99"})
        self.assertEqual((status,data),(206,media[:100]))
        self.assertEqual(received,[("/music/alpha.mp3","bytes=0-99")])
        _,_,data=self.request("/api/tracks/"+ticket["track"]["id"])
        self.assertEqual(json.loads(data)["source"],"NAS")
        _,_,another=self.request("/api/session",body={})
        _,_,data=self.request("/api/nas",headers={"Authorization":"Bearer "+json.loads(another)["token"]})
        self.assertEqual(json.loads(data),[])

    def test_provider_resolve_fallback_and_range_leases(self):
        media=(self.root/"music/alpha.mp3").read_bytes()
        received=[]
        class MediaHandler(BaseHTTPRequestHandler):
            def do_HEAD(handler):
                handler.serve(False)
            def do_GET(handler):
                handler.serve(True)
            def serve(handler,body):
                received.append((handler.path,handler.headers.get("Range"),handler.headers.get("Authorization"),handler.headers.get("Cookie")))
                if handler.path=="/failed.flac":
                    handler.send_error(503)
                    return
                start,end=0,len(media)-1
                if value:=handler.headers.get("Range"):
                    found=re.fullmatch(r"bytes=(\d+)-(\d*)",value)
                    if not found:
                        handler.send_error(416)
                        return
                    start=int(found[1]);end=min(int(found[2]) if found[2] else end,end)
                handler.send_response(206 if handler.headers.get("Range") else 200)
                handler.send_header("Content-Type","audio/mpeg")
                handler.send_header("Accept-Ranges","bytes")
                handler.send_header("Content-Length",str(end-start+1))
                if handler.headers.get("Range"):
                    handler.send_header("Content-Range",f"bytes {start}-{end}/{len(media)}")
                handler.end_headers()
                if body:
                    handler.wfile.write(media[start:end+1])
            def log_message(self,*args):
                pass
        upstream=ThreadingHTTPServer(("127.0.0.1",0),MediaHandler)
        threading.Thread(target=upstream.serve_forever,daemon=True).start()
        self.addCleanup(upstream.server_close)
        self.addCleanup(upstream.shutdown)
        base=f"http://127.0.0.1:{upstream.server_port}"
        plugins=self.root/"plugins";plugins.mkdir()
        (plugins/"test.wasm").write_bytes(provider_module(
            {"abi_version":1,"tracks":[{"id":"song-1","title":"Addon Song","artist":"WASM Test","kind":"preview","duration_ms":40000,"artwork":None}]},
            {"abi_version":1,"candidates":[{"url":base+"/failed.flac","quality":"lossless","mime_type":"audio/flac","bitrate_kbps":None},{"url":base+"/alpha.mp3","quality":"high","mime_type":"audio/mpeg","bitrate_kbps":96}]}
        ))
        self.stop()
        self.environment.update(PUMP_PLUGIN_DIR=str(plugins),PUMP_PLUGIN_ALLOWED_HOSTS="127.0.0.1")
        self.start()
        _,_,data=self.request("/api/providers/test/search?q=song")
        [track]=json.loads(data)["tracks"]
        self.assertEqual(track["provider"],{"provider_id":"test","track_id":"song-1"})
        request={"abi_version":1,"track_id":"song-1","quality":{"preferred":"high","prefer_lossless":True,"allow_fallback":True}}
        leases=[]
        for _ in range(2):
            _,_,data=self.request("/api/providers/test/resolve",body=request)
            leases.append(json.loads(data)["urls"])
        self.assertNotEqual(leases[0],leases[1])
        for urls in leases:
            with self.assertRaises(HTTPError) as failed:
                self.request(urls[0])
            self.assertEqual(failed.exception.code,502)
            status,headers,data=self.request(urls[1],headers={"Range":"bytes=100-149","Authorization":"Bearer ignored","Cookie":"test=private"})
            self.assertEqual((status,data),(206,media[100:150]))
            self.assertEqual(headers["Content-Range"],f"bytes 100-149/{len(media)}")
        self.assertTrue(all(auth is None and cookie is None for _,_,auth,cookie in received))
        with sync_playwright() as driver:
            browser=driver.chromium.launch(executable_path=os.environ.get("PUMP_TEST_CHROMIUM") or shutil.which("chromium"))
            page=browser.new_page(viewport={"width":390,"height":844})
            errors=[];page.on("pageerror",lambda error:errors.append(str(error)))
            page.route("**/api/lyrics/**",lambda route:route.fulfill(json={"synced":None,"plain":None}))
            page.goto(self.origin)
            page.locator("#tabSearch").click()
            expect(page.locator('#source option[value="plugin:test"]')).to_have_count(1)
            page.locator("#source").select_option("plugin:test")
            page.locator("#qInput").fill("song")
            page.get_by_role("button",name="Addon Song",exact=False).click()
            wait_for_audio(page,"audio => !audio.paused && audio.currentTime > .3")
            self.assertTrue(page.locator("#audio").get_attribute("src").startswith("/addon/stream/"))
            self.assertEqual(page.evaluate("selected._providerIndex"),1)
            page.evaluate("()=>{audio.pause();audio.currentTime=3;selected._providerResolvedAt=0;requestPlay();}")
            wait_for_audio(page,"audio => !audio.paused && audio.currentTime > 3.1")
            self.assertEqual(errors,[])
            browser.close()

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
            self.assertTrue(page.evaluate("() => graph.ready"))
            page.locator("#concertMode").check()
            expect(page.locator("#pitchSemitones")).to_be_enabled()
            self.assertEqual(page.evaluate("async()=>{const context=graph.context;const original=context.setSinkId;let routed=null;context.setSinkId=async id=>{routed=id;};try{await setOutputDevice('test-speakers');return routed;}finally{context.setSinkId=original;}}"),"test-speakers")
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
            page.get_by_text("Библиотека",exact=True).click()
            page.locator("#importCsv").set_input_files({"name":"exportify.csv","mimeType":"text/csv","buffer":b"Track URI,Track Name,Artist Name(s),Duration (ms)\nspotify:track:test,North Loop,PUMP Test Artist,40000\nspotify:track:test,North Loop,PUMP Test Artist,40000\n"})
            expect(page.locator("#settingsStatus")).to_contain_text("Добавлено в очередь: 2",timeout=15000)
            if not page.locator('details[data-category^="Хранилище"]').evaluate("details=>details.open"):
                page.get_by_text("Хранилище",exact=True).click()
            page.locator("#downloadQueueButton").click()
            expect(page.locator("#offlineStatus")).to_have_text("Сохранено: 0. Уже на устройстве: 1. Ошибок: 0.")
            self.assertEqual(page.evaluate("async()=>{const tracks=(await api('/api/search?source=full&q=South')).tracks;queue.push(tracks[0]);return queue.length;}"),3)
            page.locator("#downloadQueueButton").click()
            expect(page.locator("#offlineStatus")).to_have_text("Сохранено: 1. Уже на устройстве: 1. Ошибок: 0.",timeout=15000)
            expect(page.locator("#downloadsList")).to_contain_text("South Loop")
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
