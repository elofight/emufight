// ym2610_glue.cpp — NeoGeo YM2610 (Aaron Giles' ymfm) glue layer.
//
// Wraps ymfm::ym2610 with a flat C API used by Rust (sound.rs) and by
// z80_glue.c.  Replaces the gate-level FMOPNA LLE which was ~2× too slow
// for realtime even on Apple M4 Max — ymfm is a sample-accurate
// algorithmic emulator, easily 50× faster.
//
// Z80 I/O port mapping (port == arg passed to neo_ym2610_*)
//   0 → addr A   1 → data A   (FM ch 1-3, SSG)
//   2 → addr B   3 → data B   (FM ch 4-6, ADPCM-A, ADPCM-B)
//
// Sample-rate handling
//   YM2610 master clock = 8 MHz; native chip sample rate = 8 MHz / 144
//   ≈ 55 555 Hz.  We resample to 44 100 Hz using a fractional accumulator.
//
// Timers
//   ymfm calls ymfm_set_timer(tnum, duration_in_clocks).  We track the
//   countdown ourselves (decremented per generated chip sample worth of
//   master clocks) and fire engine_timer_expired() when it elapses.
//   This is what delivers timer-A/B IRQs to the Z80 driver.

#include "../../../vendor/ymfm/src/ymfm.h"
#include "../../../vendor/ymfm/src/ymfm_opn.h"

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

extern "C" void neo_z80_set_int(int level);
extern "C" int neo_z80_get_int(void);

namespace {

constexpr uint32_t MASTER_CLOCK_HZ = 8'000'000;
constexpr uint32_t OUTPUT_RATE_HZ  = 44'100;
constexpr uint32_t M68K_CLOCK_HZ   = 12'000'000;
// Marks m_last_irq in intf save_restore_state (blobs without this tag are pre-3.5).
constexpr uint32_t YM_INTF_IRQ_TAG = 0xA17C0001u;

static int debug_enabled() __attribute__((unused));
static int debug_enabled() {
    static int cached = -1;
    if (cached < 0) {
        const char* s = std::getenv("NEO_DEBUG");
        cached = (s && s[0] == '1') ? 1 : 0;
    }
    return cached;
}
#define DBG(...) do { if (debug_enabled()) std::fprintf(stderr, __VA_ARGS__); } while (0)

// Per-source mute mask, parsed once from NEO_MUTE env var.
// Format: comma-separated list of: fm, ssg, adpcma, adpcmb
// e.g. NEO_MUTE=ssg,adpcmb  -> mute SSG and ADPCM-B in the final mix.
// NOTE: ADPCM-A/B are mixed inside ymfm into data[0]/data[1] together with
// FM, so muting them at the API output is approximate (zeros all FM too).
// Useful values in practice: "fm" (zero FM+ADPCM channels), "ssg".
enum : uint32_t {
    MUTE_FM     = 1u << 0,
    MUTE_SSG    = 1u << 1,
};
static uint32_t mute_mask() {
    static uint32_t cached = ~0u;
    if (cached == ~0u) {
        cached = 0;
        if (const char* s = std::getenv("NEO_MUTE")) {
            std::string env(s);
            auto has = [&](const char* w){ return env.find(w) != std::string::npos; };
            if (has("fm"))     cached |= MUTE_FM;
            if (has("ssg"))    cached |= MUTE_SSG;
            DBG("[YM2610-ymfm] NEO_MUTE='%s' -> mask=0x%x\n",
                         s, cached);
        }
    }
    return cached;
}

class NeoYmInterface : public ymfm::ymfm_interface {
public:
    NeoYmInterface()  = default;
    ~NeoYmInterface() override = default;

    // ── timer plumbing ─────────────────────────────────────────────────
    // `duration_in_clocks` is in master clocks.  Negative → cancel.
    void ymfm_set_timer(uint32_t tnum, int32_t duration_in_clocks) override {
        if (tnum > 1) return;
        if (duration_in_clocks < 0) {
            m_timer_clocks[tnum] = -1; // disabled
        } else {
            m_timer_clocks[tnum] = duration_in_clocks;
        }
        DBG("[YM2610-ymfm] set_timer t=%u dur=%d\n", tnum, duration_in_clocks);
    }

    // Called after every generated chip sample (= 144 master clocks at
    // MED fidelity).  Fires engine_timer_expired() for any timer whose
    // countdown has reached zero.
    void advance_clocks(int32_t clocks) {
        for (uint32_t t = 0; t < 2; t++) {
            if (m_timer_clocks[t] < 0) continue;
            m_timer_clocks[t] -= clocks;
            if (m_timer_clocks[t] <= 0) {
                m_timer_clocks[t] = -1; // ymfm will re-arm via ymfm_set_timer
                m_timer_expire_count[t]++;
                if (m_engine) m_engine->engine_timer_expired(t);
            }
        }
    }
    uint64_t timer_expire_count(int t) const { return m_timer_expire_count[t & 1]; }

    // ── IRQ to Z80 ─────────────────────────────────────────────────────
    void bind_z80_int(int32_t* ptr) { m_z80_int = ptr; }

    void ymfm_update_irq(bool asserted) override {
        int level = asserted ? 1 : 0;
        if (level != m_last_irq) {
            m_last_irq = level;
            if (asserted) m_irq_assert_count++;
            if (m_z80_int) *m_z80_int = level;
            else neo_z80_set_int(level);
        }
    }
    uint64_t irq_assert_count() const { return m_irq_assert_count; }

    // ── busy flag (not used by NeoGeo Z80 driver in any timing-critical
    //    way; default no-op behaviour is fine) ──────────────────────────

    // ── ADPCM ROM access ───────────────────────────────────────────────
    uint8_t ymfm_external_read(ymfm::access_class type, uint32_t offset) override {
        if (type == ymfm::ACCESS_ADPCM_A) {
            if (offset < m_adpcm_a.size()) return m_adpcm_a[offset];
        } else if (type == ymfm::ACCESS_ADPCM_B) {
            if (offset < m_adpcm_b.size()) return m_adpcm_b[offset];
        }
        return 0;
    }

    void set_adpcm_a(const uint8_t* data, size_t size) {
        m_adpcm_a.assign(data, data + size);
    }
    void set_adpcm_b(const uint8_t* data, size_t size) {
        m_adpcm_b.assign(data, data + size);
    }

    void reset_state() {
        m_timer_clocks[0] = -1;
        m_timer_clocks[1] = -1;
        m_last_irq = -1;
        m_irq_assert_count = 0;
        m_timer_expire_count[0] = 0;
        m_timer_expire_count[1] = 0;
    }

    void reset_irq_cache() {
        m_last_irq = -1;
    }

    void sync_irq_edge_from_line() {
        m_last_irq = m_z80_int ? *m_z80_int : neo_z80_get_int();
    }

    // Capture/restore timer countdown and IRQ edge detector for rollback.
    // ROM data (m_adpcm_a/b) is excluded — it is reloaded from disk.
    //
    // Layout (new): timer[0], timer[1], YM_INTF_IRQ_TAG, m_last_irq
    // Layout (old): timer[0], timer[1]  — m_last_irq re-seeded from Z80 line
    void save_restore_state(ymfm::ymfm_saved_state& st) {
        st.save_restore(m_timer_clocks[0]);
        st.save_restore(m_timer_clocks[1]);
        if (st.saving()) {
            uint32_t tag = YM_INTF_IRQ_TAG;
            st.save_restore(tag);
            st.save_restore(m_last_irq);
        } else {
            uint32_t tag = 0;
            const int32_t off = st.m_offset;
            if (off + 4 <= (int32_t)st.m_buffer.size()) {
                const uint8_t* p = st.m_buffer.data() + off;
                tag = uint32_t(p[0]) | (uint32_t(p[1]) << 8) |
                      (uint32_t(p[2]) << 16) | (uint32_t(p[3]) << 24);
            }
            if (tag == YM_INTF_IRQ_TAG) {
                st.m_offset += 4;
                st.save_restore(m_last_irq);
            } else {
                sync_irq_edge_from_line();
            }
        }
    }

private:
    int32_t m_timer_clocks[2] = { -1, -1 };
    int32_t* m_z80_int        = nullptr;
    int     m_last_irq        = -1;
    uint64_t m_irq_assert_count = 0;
    uint64_t m_timer_expire_count[2] = {0, 0};
    std::vector<uint8_t> m_adpcm_a;
    std::vector<uint8_t> m_adpcm_b;
};

constexpr size_t RING_SZ = 8192; // ≥ 2 frames @ 44.1kHz/60fps (735*2=1470)
constexpr uint64_t SAMPLES_PER_M68K_Q32 = (uint64_t(OUTPUT_RATE_HZ) << 32) / M68K_CLOCK_HZ;
// Legacy blobs appended YM_AUX_MAGIC + last_sample + ring after resamp/tick pos.
constexpr uint32_t YM_AUX_MAGIC = 0x4E454F58u; // 'NEOX'

struct NeoYmState {
    NeoYmInterface* intf;
    ymfm::ym2610*   chip;
    uint32_t        chip_rate;
    int32_t         clocks_per_sample;
    uint64_t        resamp_pos;
    uint64_t        resamp_step;
    ymfm::ym2610::output_data last_sample;
    uint64_t        tick_pos;
    float           ring[RING_SZ];
    size_t          ring_head;
    size_t          ring_tail;
    size_t          ring_count;
    uint64_t        out_samples_generated;
};

static void clear_output_ring(NeoYmState* state) {
    state->last_sample.clear();
    state->ring_head = state->ring_tail = state->ring_count = 0;
}

// Consume and discard a legacy YM_AUX_MAGIC tail if present in the blob.
static void skip_legacy_aux_tail(ymfm::ymfm_saved_state& st) {
    if (st.saving()) return;
    if (st.m_offset + 4 > (int32_t)st.m_buffer.size()) return;
    const int32_t off = st.m_offset;
    uint32_t magic = 0;
    st.save_restore(magic);
    if (magic != YM_AUX_MAGIC) {
        st.m_offset = off;
        return;
    }
    for (int i = 0; i < 3; i++) {
        int32_t sample = 0;
        st.save_restore(sample);
    }
    uint32_t rh = 0, rt = 0, rc = 0;
    st.save_restore(rh);
    st.save_restore(rt);
    st.save_restore(rc);
    for (size_t i = 0; i < RING_SZ; i++) {
        uint32_t bits = 0;
        st.save_restore(bits);
    }
}

inline void produce_one_sample(NeoYmState* state) {
    constexpr uint64_t ONE = uint64_t(1) << 32;
    // Pull enough chip samples to cover the output sample interval.
    state->resamp_pos += state->resamp_step;
    while (state->resamp_pos >= ONE) {
        state->resamp_pos -= ONE;
        state->chip->generate(&state->last_sample, 1);
        state->intf->advance_clocks(state->clocks_per_sample);
    }
    const uint32_t mask = mute_mask();
    int32_t fm_l = (mask & MUTE_FM)  ? 0 : state->last_sample.data[0];
    int32_t fm_r = (mask & MUTE_FM)  ? 0 : state->last_sample.data[1];
    int32_t ssg  = (mask & MUTE_SSG) ? 0 : state->last_sample.data[2];
    DBG("[YM2610-ymfm] peak FM_L=%d FM_R=%d SSG=%d (raw int)\n",
        state->last_sample.data[0], state->last_sample.data[1], state->last_sample.data[2]);
    float mono = (float(fm_l + fm_r) * 0.5f + float(ssg)) / 32768.0f;
    if (mono > 1.0f)  mono = 1.0f;
    if (mono < -1.0f) mono = -1.0f;
    if (state->ring_count >= RING_SZ) {
        // Drop oldest to make room (shouldn't happen in practice).
        state->ring_tail = (state->ring_tail + 1) % RING_SZ;
        state->ring_count--;
    }
    state->ring[state->ring_head] = mono;
    state->ring_head = (state->ring_head + 1) % RING_SZ;
    state->ring_count++;
}

} // namespace

// ──────────────────────────────────────────────────────────────────────────
//  Public C API
// ──────────────────────────────────────────────────────────────────────────
extern "C" {

void* neo_ym2610_create(void) {
    NeoYmState* state = new NeoYmState();
    state->intf = new NeoYmInterface();
    state->chip = new ymfm::ym2610(*state->intf);
    state->chip->set_fidelity(ymfm::OPN_FIDELITY_MED);
    state->chip_rate = state->chip->sample_rate(MASTER_CLOCK_HZ);
    state->clocks_per_sample = MASTER_CLOCK_HZ / state->chip_rate;
    state->resamp_step = (uint64_t(state->chip_rate) << 32) / OUTPUT_RATE_HZ;
    state->resamp_pos = 0;
    state->tick_pos = 0;
    state->ring_head = 0;
    state->ring_tail = 0;
    state->ring_count = 0;
    state->out_samples_generated = 0;
    state->last_sample.clear();
    return state;
}

void neo_ym2610_destroy(void* ptr) {
    if (!ptr) return;
    NeoYmState* state = static_cast<NeoYmState*>(ptr);
    state->intf->bind_z80_int(nullptr);
    delete state->chip;
    delete state->intf;
    delete state;
}

void neo_ym2610_reset(void* ptr) {
    if (!ptr) return;
    NeoYmState* state = static_cast<NeoYmState*>(ptr);
    delete state->chip;
    state->chip = new ymfm::ym2610(*state->intf);
    state->chip->set_fidelity(ymfm::OPN_FIDELITY_MED);
    state->intf->reset_state();
    state->resamp_pos = 0;
    state->tick_pos = 0;
    state->ring_head = state->ring_tail = state->ring_count = 0;
    state->last_sample.clear();
}

void neo_ym2610_write(void* ptr, int port, int data) {
    if (!ptr) return;
    NeoYmState* state = static_cast<NeoYmState*>(ptr);
    
    static uint64_t s_writes[4] = {0,0,0,0};
    static uint64_t s_fm_freq_writes_a = 0;
    static uint64_t s_fm_freq_writes_b = 0;
    static uint64_t s_keyon_writes = 0;
    static uint64_t s_keyon_ch[8] = {0};
    static uint64_t s_keyoff_ch[8] = {0};
    static uint64_t s_adpcma_on[6]   = {0};
    static uint64_t s_adpcma_off[6]  = {0};
    static uint16_t s_adpcma_start[6] = {0};
    static uint16_t s_adpcma_end[6]   = {0};
    static uint8_t  s_adpcma_panvol[6]= {0};
    static uint8_t  s_adpcma_master   = 0;
    static uint8_t  last_addr_a = 0, last_addr_b = 0;
    static uint64_t s_reg_hits_a[256] = {0};
    static uint64_t s_reg_hits_b[256] = {0};
    static uint8_t  s_ssg_last[16]    = {0};
    static int      s_ssg_seen[16]    = {0};
    static uint8_t  s_timer_24 = 0, s_timer_25 = 0, s_timer_26 = 0, s_timer_27 = 0;
    int p = port & 3;
    s_writes[p]++;
    if (p == 0) last_addr_a = (uint8_t)data;
    else if (p == 2) last_addr_b = (uint8_t)data;
    else if (p == 1) {
        s_reg_hits_a[last_addr_a]++;
        if (last_addr_a < 0x10) { s_ssg_last[last_addr_a] = (uint8_t)data; s_ssg_seen[last_addr_a] = 1; }
        if (last_addr_a == 0x24) s_timer_24 = (uint8_t)data;
        else if (last_addr_a == 0x25) s_timer_25 = (uint8_t)data;
        else if (last_addr_a == 0x26) s_timer_26 = (uint8_t)data;
        else if (last_addr_a == 0x27) s_timer_27 = (uint8_t)data;
        if (last_addr_a == 0x28) {
            s_keyon_writes++;
            unsigned ch = (unsigned)(data & 0x07);
            if (data & 0xF0) s_keyon_ch[ch & 7]++;
            else             s_keyoff_ch[ch & 7]++;
        }
        if (last_addr_a >= 0xA0 && last_addr_a <= 0xA6) s_fm_freq_writes_a++;
    } else /* p == 3 */ {
        s_reg_hits_b[last_addr_b]++;
        if (last_addr_b >= 0xA0 && last_addr_b <= 0xA6) s_fm_freq_writes_b++;
        uint8_t r = last_addr_b;
        uint8_t v = (uint8_t)data;
        if (r == 0x00) {
            uint8_t mask = v & 0x3F;
            if (v & 0x80) {
                for (int c = 0; c < 6; c++) if (mask & (1u<<c)) s_adpcma_off[c]++;
            } else {
                for (int c = 0; c < 6; c++) if (mask & (1u<<c)) s_adpcma_on[c]++;
            }
        } else if (r == 0x01) {
            s_adpcma_master = v & 0x3F;
            (void)s_adpcma_master; // captured for debug tracing only
        } else if (r >= 0x08 && r <= 0x0D) {
            s_adpcma_panvol[r - 0x08] = v;
        } else if (r >= 0x10 && r <= 0x15) {
            int c = r - 0x10; s_adpcma_start[c] = (s_adpcma_start[c] & 0xFF00) | v;
        } else if (r >= 0x18 && r <= 0x1D) {
            int c = r - 0x18; s_adpcma_start[c] = (uint16_t)((s_adpcma_start[c] & 0x00FF) | ((uint16_t)v << 8));
        } else if (r >= 0x20 && r <= 0x25) {
            int c = r - 0x20; s_adpcma_end[c] = (s_adpcma_end[c] & 0xFF00) | v;
        } else if (r >= 0x28 && r <= 0x2D) {
            int c = r - 0x28; s_adpcma_end[c] = (uint16_t)((s_adpcma_end[c] & 0x00FF) | ((uint16_t)v << 8));
        }
    }
    
    uint64_t total = s_writes[0]+s_writes[1]+s_writes[2]+s_writes[3];
    if (debug_enabled() && (total == 1 || total == 100 || total == 1000 || total % 5000 == 0)) {
        static double s_last_secs = 0.0;
        static uint64_t s_last_total = 0, s_last_irqs = 0, s_last_keyon = 0, s_last_fmfA = 0, s_last_fmfB = 0, s_last_outsamp = 0, s_last_texpA = 0, s_last_texpB = 0;
        struct timespec ts; clock_gettime(CLOCK_MONOTONIC, &ts);
        double now_s = (double)ts.tv_sec + (double)ts.tv_nsec * 1e-9;
        double dt = now_s - s_last_secs;
        if (dt < 0.01) dt = 0.01;
        uint64_t irqs_now = state->intf->irq_assert_count();
        uint64_t outs_now = state->out_samples_generated;
        uint64_t texpA = state->intf->timer_expire_count(0);
        uint64_t texpB = state->intf->timer_expire_count(1);
        std::fprintf(stderr,
            "[YM2610-ymfm] RATES dt=%.2fs out_samples/s=%.0f writes/s=%.0f IRQs/s=%.1f TexpA/s=%.1f TexpB/s=%.1f FMkeyon/s=%.1f FMfreqA/s=%.1f FMfreqB/s=%.1f | TimerA period=%u (hi=$%02X lo=$%02X) TimerB=$%02X reg27=$%02X\n",
            dt,
            (double)(outs_now - s_last_outsamp)/dt,
            (double)(total - s_last_total)/dt,
            (double)(irqs_now - s_last_irqs)/dt,
            (double)(texpA - s_last_texpA)/dt,
            (double)(texpB - s_last_texpB)/dt,
            (double)(s_keyon_writes - s_last_keyon)/dt,
            (double)(s_fm_freq_writes_a - s_last_fmfA)/dt,
            (double)(s_fm_freq_writes_b - s_last_fmfB)/dt,
            (unsigned)((s_timer_24 << 2) | (s_timer_25 & 3)),
            s_timer_24, s_timer_25, s_timer_26, s_timer_27);
        s_last_secs = now_s;
        s_last_total = total; s_last_irqs = irqs_now;
        s_last_keyon = s_keyon_writes;
        s_last_fmfA = s_fm_freq_writes_a; s_last_fmfB = s_fm_freq_writes_b;
        s_last_outsamp = outs_now;
        s_last_texpA = texpA; s_last_texpB = texpB;
        std::fprintf(stderr,
            "[YM2610-ymfm] writes total=%llu portA addr=%llu data=%llu portB addr=%llu data=%llu keyon=%llu FMfreqA=%llu FMfreqB=%llu IRQs=%llu\n",
            (unsigned long long)total,
            (unsigned long long)s_writes[0], (unsigned long long)s_writes[1],
            (unsigned long long)s_writes[2], (unsigned long long)s_writes[3],
            (unsigned long long)s_keyon_writes,
            (unsigned long long)s_fm_freq_writes_a,
            (unsigned long long)s_fm_freq_writes_b,
            (unsigned long long)state->intf->irq_assert_count());
        std::fflush(stderr);
    }
    
    state->chip->write(static_cast<uint32_t>(p), static_cast<uint8_t>(data & 0xff));
}

int neo_ym2610_read(void* ptr, int port) {
    if (!ptr) return 0;
    NeoYmState* state = static_cast<NeoYmState*>(ptr);
    return state->chip->read(static_cast<uint32_t>(port & 3));
}

void neo_ym2610_generate(void* ptr, float* buf, int n) {
    if (!ptr) return;
    NeoYmState* state = static_cast<NeoYmState*>(ptr);
    state->out_samples_generated += (uint64_t)n;
    for (int i = 0; i < n; i++) {
        if (state->ring_count > 0) {
            float s = state->ring[state->ring_tail];
            state->ring_tail = (state->ring_tail + 1) % RING_SZ;
            state->ring_count--;
            buf[i] = s;
        } else {
            buf[i] = 0.0f;
        }
    }
}

int neo_ym2610_ring_count(void* ptr) {
    if (!ptr) return 0;
    return (int)static_cast<NeoYmState*>(ptr)->ring_count;
}

// Re-seed the IRQ edge detector from the live Z80 /INT line and reconcile
// ymfm status before each frame's batched CPU run.  Keeps continuous play and
// post-restore simulation aligned at frame boundaries.
void neo_ym2610_begin_frame(void* ptr) {
    if (!ptr) return;
    NeoYmState* state = static_cast<NeoYmState*>(ptr);
    state->intf->sync_irq_edge_from_line();
    state->intf->ymfm_sync_check_interrupts();
}

void neo_ym2610_bind_z80_int(void* ptr, int32_t* z80_int) {
    if (!ptr) return;
    static_cast<NeoYmState*>(ptr)->intf->bind_z80_int(z80_int);
}

void neo_ym2610_tick(void* ptr, int m68k_cycles) {
    if (!ptr) return;
    NeoYmState* state = static_cast<NeoYmState*>(ptr);
    if (m68k_cycles <= 0) return;
    constexpr uint64_t ONE = uint64_t(1) << 32;
    state->tick_pos += SAMPLES_PER_M68K_Q32 * (uint64_t)m68k_cycles;
    while (state->tick_pos >= ONE) {
        state->tick_pos -= ONE;
        produce_one_sample(state);
    }
}

void neo_ym2610_load_adpcm_a(void* ptr, const uint8_t* data, int size) {
    if (!ptr) return;
    NeoYmState* state = static_cast<NeoYmState*>(ptr);
    if (size <= 0 || !data) return;
    state->intf->set_adpcm_a(data, static_cast<size_t>(size));
}

void neo_ym2610_load_adpcm_b(void* ptr, const uint8_t* data, int size) {
    if (!ptr) return;
    NeoYmState* state = static_cast<NeoYmState*>(ptr);
    if (size <= 0 || !data) return;
    state->intf->set_adpcm_b(data, static_cast<size_t>(size));
}

void neo_ym2610_load_vrom(void* ptr, const uint8_t* data, int size) {
    neo_ym2610_load_adpcm_a(ptr, data, size);
}

void neo_ym2610_save_state(void* ptr, uint8_t** out_buf, int* out_size) {
    if (!ptr) return;
    NeoYmState* state = static_cast<NeoYmState*>(ptr);
    // Read-only capture: do not touch m_last_irq here.  begin_frame() already
    // reconciles the edge detector with the Z80 int_line each frame; re-syncing
    // during save mutates live state and makes back-to-back snapshots diverge on
    // long-lived instances (frame 44→45 rollback desync).
    // The output ring and last_sample are deliberately NOT saved — they hold
    // already-decimated audio, not emulation state (matches YM2151 / CPS).
    std::vector<uint8_t> buf;
    ymfm::ymfm_saved_state st(buf, true);
    state->chip->save_restore(st);
    state->intf->save_restore_state(st);
    uint32_t rp_lo = (uint32_t)state->resamp_pos, rp_hi = (uint32_t)(state->resamp_pos >> 32);
    uint32_t tp_lo = (uint32_t)state->tick_pos,   tp_hi = (uint32_t)(state->tick_pos   >> 32);
    st.save_restore(rp_lo); st.save_restore(rp_hi);
    st.save_restore(tp_lo); st.save_restore(tp_hi);
    *out_size = (int)buf.size();
    uint8_t* p = (uint8_t*)std::malloc(buf.empty() ? 1 : buf.size());
    if (!buf.empty()) std::memcpy(p, buf.data(), buf.size());
    *out_buf = p;
}

void neo_ym2610_load_state(void* ptr, const uint8_t* in_buf, int in_size) {
    if (!ptr) return;
    NeoYmState* state = static_cast<NeoYmState*>(ptr);
    if (!in_buf || in_size <= 0) return;
    std::vector<uint8_t> buf(in_buf, in_buf + in_size);
    ymfm::ymfm_saved_state st(buf, false);
    state->chip->save_restore(st);
    state->intf->save_restore_state(st);
    uint32_t rp_lo = 0, rp_hi = 0, tp_lo = 0, tp_hi = 0;
    st.save_restore(rp_lo); st.save_restore(rp_hi);
    st.save_restore(tp_lo); st.save_restore(tp_hi);
    state->resamp_pos = ((uint64_t)rp_hi << 32) | rp_lo;
    state->tick_pos   = ((uint64_t)tp_hi << 32) | tp_lo;
    // Drop any buffered output; regenerated from chip state after rollback.
    clear_output_ring(state);
    skip_legacy_aux_tail(st);
    // m_last_irq is restored from the blob (must match the Z80 int_line
    // snapshot).  Do not re-seed from the line here — that drops the saved
    // edge detector and can pulse /INT on the first post-restore frame.
    state->intf->ymfm_sync_check_interrupts();
}

void neo_ym2610_free_state_buf(uint8_t* buf) {
    std::free(buf);
}

// ── diagnostic counters (for tests / debug; NOT part of save state) ──
uint64_t neo_ym2610_get_irq_assert_count(void* ptr) {
    if (!ptr) return 0;
    return static_cast<NeoYmState*>(ptr)->intf->irq_assert_count();
}

uint64_t neo_ym2610_get_timer_expire_count(void* ptr, int timer_idx) {
    if (!ptr) return 0;
    return static_cast<NeoYmState*>(ptr)->intf->timer_expire_count(timer_idx & 1);
}

} // extern "C"
