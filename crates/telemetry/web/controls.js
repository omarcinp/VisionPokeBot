'use strict';
// Shared by the game grid and detailed report. Each tab gets one exclusive
// lease; input requests are serialized and carry monotonic sequence numbers.
class GameControls {
  constructor(root, path) {
    this.root = root; this.path = path; this.held = new Set(); this.sequence = 0;
    this.owner = Array.from(crypto.getRandomValues(new Uint32Array(4)), n => n.toString(16)).join('-');
    this.revision=0; this.actionPending=false; this.active = false; this.closed = false; this.sending = false; this.lastBeat = 0;
    root.innerHTML = `<div class="control-bar"><span class="control-state">Connecting…</span><button data-action="take" class="primary">Take control</button><button data-action="release" hidden>Release controls</button><button data-action="resume" hidden>Resume bot</button><button data-action="stop">Stop bot</button></div>
      <div class="manual-pad" hidden><div class="directions"><button data-key="Up" aria-label="Up">↑</button><button data-key="Left" aria-label="Left">←</button><button data-key="Down" aria-label="Down">↓</button><button data-key="Right" aria-label="Right">→</button></div><div class="face-buttons"><button data-key="B">B</button><button data-key="A">A</button></div><div class="system-buttons"><button data-key="L">L</button><button data-key="Select">Select</button><button data-key="Start">Start</button><button data-key="R">R</button></div></div>
      <p class="control-help">Take control to pause the bot. Arrow keys · Z = A · X = B · Enter = Start · Shift = Select · Q / W = L / R</p><p class="control-error" role="alert"></p>`;
    this.error = root.querySelector('.control-error');
    for (const el of root.querySelectorAll('[data-action]')) el.onclick = () => this.action(el.dataset.action);
    this.pointers = new Map();
    for (const el of root.querySelectorAll('[data-key]')) {
      el.onpointerdown = e => { if (!this.active) return; e.preventDefault(); el.setPointerCapture(e.pointerId); this.pointers.set(e.pointerId,el.dataset.key); this.held.add(el.dataset.key); this.paint(); this.send(); };
      const release = e => { const key = this.pointers.get(e.pointerId); this.pointers.delete(e.pointerId); if (!Array.from(this.pointers.values()).includes(key)) this.held.delete(key); this.paint(); this.send(); };
      el.onpointerup = release; el.onpointercancel = release; el.onlostpointercapture = release;
    }
    const keys = {ArrowUp:'Up',ArrowDown:'Down',ArrowLeft:'Left',ArrowRight:'Right',KeyZ:'A',KeyX:'B',Enter:'Start',ShiftLeft:'Select',ShiftRight:'Select',KeyQ:'L',KeyW:'R'};
    this.key = e => {
      const b = keys[e.code];
      if (!this.active || !b || /INPUT|SELECT|TEXTAREA/.test(e.target.tagName) || e.target.isContentEditable) return;
      e.preventDefault(); if (e.repeat) return;
      if (e.type === 'keydown') this.held.add(b); else this.held.delete(b);
      this.paint(); this.send();
    };
    this.blur = () => { this.held.clear(); this.pointers.clear(); this.paint(); this.send(); };
    this.visibility = () => { if (document.hidden) this.blur(); };
    this.unload = () => this.releaseOnExit();
    window.addEventListener('keydown',this.key); window.addEventListener('keyup',this.key);
    window.addEventListener('blur',this.blur); window.addEventListener('pagehide',this.unload);
    document.addEventListener('visibilitychange',this.visibility);
    this.timer = setInterval(() => {
      if (this.active && !document.hidden) this.send();
      if (Date.now()-this.lastBeat>1000) { this.lastBeat=Date.now(); this.refresh(); }
    },80);
    this.refresh();
  }
  async request(body) {
    const response = await fetch(this.path+'api/control',{cache:'no-store',signal:AbortSignal.timeout(4000),
      ...(body ? {method:'POST',headers:{'Content-Type':'application/json','X-Pokebot-Control':'1'},body:JSON.stringify({...body,owner:this.owner})} : {})});
    const value = await response.json(); if (!response.ok) throw Error(value.error || 'Controls unavailable'); return value;
  }
  show(status) {
    this.root.querySelector('.control-state').textContent = {bot:'Bot playing',manual:this.active?'You have control':'Another browser has control',stopped:'Bot stopped'}[status.mode] || status.mode;
    this.root.querySelector('[data-action="take"]').hidden = this.active;
    this.root.querySelector('[data-action="take"]').disabled = status.mode==='manual' && !this.active;
    this.root.querySelector('[data-action="release"]').hidden = !this.active;
    this.root.querySelector('[data-action="resume"]').hidden = !status.can_resume || status.mode==='bot';
    this.root.querySelector('[data-action="stop"]').disabled = status.mode==='stopped';
    this.root.querySelector('.manual-pad').hidden = !this.active;
  }
  async refresh() { if (this.closed || this.actionPending) return; const revision=this.revision; try { const status=await this.request(); if(revision!==this.revision || this.closed) return; if(status.mode!=='manual') this.active=false; this.show(status); } catch(e) { this.error.textContent=e.message; } }
  async action(action) {
    if(this.actionPending || this.closed) return;
    this.actionPending=true; this.revision++; this.error.textContent='';
    try {
      const status=await this.request({action});
      this.active=action==='take'; this.held.clear(); this.paint(); this.show(status);
    } catch(e) { this.error.textContent=e.message; }
    finally { this.actionPending=false; }
  }
  async send() {
    if (!this.active || this.closed || this.sending || this.actionPending) return;
    this.sending=true; const revision=this.revision;
    try { await this.request({action:'input',sequence:++this.sequence,buttons:[...this.held]}); }
    catch(e) { if(revision!==this.revision || this.closed) return; this.error.textContent=e.message; this.active=false; this.held.clear(); this.paint(); await this.refresh(); }
    finally { this.sending=false; }
  }
  paint() { for(const el of this.root.querySelectorAll('[data-key]')) el.classList.toggle('pressed',this.held.has(el.dataset.key)); }
  releaseOnExit() {
    if (!this.active) return;
    this.active=false;
    fetch(this.path+'api/control',{method:'POST',keepalive:true,headers:{'Content-Type':'application/json','X-Pokebot-Control':'1'},body:JSON.stringify({action:'release',owner:this.owner})}).catch(()=>{});
  }
  close() {
    this.releaseOnExit(); this.closed=true; clearInterval(this.timer);
    window.removeEventListener('keydown',this.key); window.removeEventListener('keyup',this.key);
    window.removeEventListener('blur',this.blur); window.removeEventListener('pagehide',this.unload);
    document.removeEventListener('visibilitychange',this.visibility);
  }
}
window.GameControls=GameControls;
