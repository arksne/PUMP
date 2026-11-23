// Контракт настоящего Rust/WASM-провайдера, без сетевых обращений к SoundHelix.
import {readFile} from "node:fs/promises";
import test from "node:test";
import assert from "node:assert/strict";

const path=process.env.PROVIDER_WASM||new URL("./target/wasm32-unknown-unknown/release/pump_provider_soundhelix.wasm",import.meta.url);
const module=await WebAssembly.compile(await readFile(path));
async function call(name,request){
 const instance=await WebAssembly.instantiate(module,{});
 const {exports}=instance;
 assert.equal(exports.plugin_abi_version(),1);
 const bytes=new TextEncoder().encode(JSON.stringify(request));
 const pointer=exports.alloc(bytes.length);
 new Uint8Array(exports.memory.buffer,pointer,bytes.length).set(bytes);
 const result=exports[name](pointer,bytes.length);
 const offset=Number(result>>32n),length=Number(result&0xffffffffn);
 return JSON.parse(new TextDecoder().decode(new Uint8Array(exports.memory.buffer,offset,length)));
}
test("модуль не требует host-импортов",()=>assert.deepEqual(WebAssembly.Module.imports(module),[]));
test("поиск учитывает все слова запроса и limit",async()=>{
 const response=await call("search",{abi_version:1,query:"soundhelix song",limit:2});
 assert.deepEqual(response.tracks.map(track=>track.id),["song-1","song-2"]);
 assert.equal((await call("search",{abi_version:1,query:"soundhelix 3",limit:20})).tracks[0].id,"song-3");
 assert.deepEqual((await call("search",{abi_version:1,query:"нет совпадений",limit:20})).tracks,[]);
});
test("resolve возвращает полную запись только для известного ID",async()=>{
 const response=await call("resolve",{abi_version:1,track_id:"song-2",quality:{preferred:"high",prefer_lossless:true,allow_fallback:true}});
 assert.equal(response.candidates[0].url,"https://www.soundhelix.com/examples/mp3/SoundHelix-Song-2.mp3");
 assert.equal(response.candidates[0].quality,"standard");
 assert.ok((await call("resolve",{abi_version:1,track_id:"missing"})).error);
});
test("несовместимый ABI и пустой поиск дают явную ошибку",async()=>{
 assert.ok((await call("search",{abi_version:2,query:"song",limit:20})).error);
 assert.ok((await call("search",{abi_version:1,query:" ",limit:20})).error);
});
