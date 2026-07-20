// ym2151_glue.cpp — CPS1 YM2151 (OPM) glue over Aaron Giles' ymfm.
//
// Wraps ymfm::ym2151 with a flat C API used by the Rust CPS1 sound board
// (src/cps/sound.rs).  The YM2151 provides all of the FM music and most of
// the sound effects on CPS1 hardware; the OKI MSM6295 (handled in Rust) only
// plays PCM voice samples.
//
// Timing model
//   YM2151 master clock = 3.579545 MHz; native chip sample rate = clock / 64
//   ≈ 55 930 Hz.  The Rust side steps the Z80 sound CPU and calls
//   neo_ym2151_advance() once per elapsed 64-clock chip sample, keeping the
//   chip in lockstep with the CPU so timer-A/B IRQs are delivered with the
//   correct phase.  Generated audio is decimated to 44 100 Hz and buffered in
//   a ring that neo_ym2151_generate() drains each frame.
//
// IRQ
//   ymfm calls ymfm_update_irq() when the timer IRQ line changes.  We latch
//   the level; the Rust side polls neo_ym2151_irq() after each advance and
//   drives the Z80 INT line accordingly (no C->Rust callback needed).

#include "../../../vendor/ymfm/src/ymfm.h"
#include "../../../vendor/ymfm/src/ymfm_opm.h"

#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <vector>

namespace {

constexpr uint32_t MASTER_CLOCK_HZ = 3'579'545;
constexpr uint32_t OUTPUT_RATE_HZ  = 44'100;
constexpr size_t   RING_SZ         = 8192;

class OpmInterface : public ymfm::ymfm_interface {
public:
    // `duration_in_clocks` is in master clocks; negative cancels the timer.
    void ymfm_set_timer(uint32_t tnum, int32_t duration_in_clocks) override {
        if (tnum > 1) return;
        m_timer_clocks[tnum] = (duration_in_clocks < 0) ? -1 : duration_in_clocks;
    }

    // Called once per generated chip sample (= clocks_per_sample master clocks).
    void advance_clocks(int32_t clocks) {
        for (uint32_t t = 0; t < 2; t++) {
            if (m_timer_clocks[t] < 0) continue;
            m_timer_clocks[t] -= clocks;
            if (m_timer_clocks[t] <= 0) {
                m_timer_clocks[t] = -1; // ymfm re-arms via ymfm_set_timer
                if (m_engine) m_engine->engine_timer_expired(t);
            }
        }
    }

    void ymfm_update_irq(bool asserted) override {
        m_irq = asserted ? 1 : 0;
    }
    int irq() const { return m_irq; }

    void reset_state() {
        m_timer_clocks[0] = -1;
        m_timer_clocks[1] = -1;
        m_irq = 0;
    }

    // Save/restore the interface-side timer + IRQ latch (chip registers are
    // handled separately by ym2151::save_restore).
    void save_restore_state(ymfm::ymfm_saved_state& st) {
        st.save_restore(m_timer_clocks[0]);
        st.save_restore(m_timer_clocks[1]);
        int32_t irq = m_irq;
        st.save_restore(irq);
        m_irq = irq;
    }

private:
    int32_t m_timer_clocks[2] = { -1, -1 };
    int     m_irq             = 0;
};

struct OpmState {
    OpmInterface*  intf;
    ymfm::ym2151*  chip;
    uint32_t       chip_rate;
    int32_t        clocks_per_sample;
    // Decimation accumulator: output samples produced per chip sample, Q32.
    uint64_t       out_step;   // (OUTPUT_RATE << 32) / chip_rate
    uint64_t       out_pos;
    ymfm::ym2151::output_data last_sample;
    float          ring[RING_SZ];
    size_t         ring_head;
    size_t         ring_tail;
    size_t         ring_count;
};

inline void push_ring(OpmState* s, float v) {
    if (s->ring_count >= RING_SZ) {
        s->ring_tail = (s->ring_tail + 1) % RING_SZ;
        s->ring_count--;
    }
    s->ring[s->ring_head] = v;
    s->ring_head = (s->ring_head + 1) % RING_SZ;
    s->ring_count++;
}

} // namespace

extern "C" {

void* neo_ym2151_create(void) {
    OpmState* s = new OpmState();
    s->intf = new OpmInterface();
    s->chip = new ymfm::ym2151(*s->intf);
    s->chip_rate = s->chip->sample_rate(MASTER_CLOCK_HZ);
    if (s->chip_rate == 0) s->chip_rate = MASTER_CLOCK_HZ / 64;
    s->clocks_per_sample = (int32_t)(MASTER_CLOCK_HZ / s->chip_rate);
    s->out_step = (uint64_t(OUTPUT_RATE_HZ) << 32) / s->chip_rate;
    s->out_pos = 0;
    s->ring_head = s->ring_tail = s->ring_count = 0;
    s->last_sample.clear();
    return s;
}

void neo_ym2151_destroy(void* ptr) {
    if (!ptr) return;
    OpmState* s = static_cast<OpmState*>(ptr);
    delete s->chip;
    delete s->intf;
    delete s;
}

void neo_ym2151_reset(void* ptr) {
    if (!ptr) return;
    OpmState* s = static_cast<OpmState*>(ptr);
    delete s->chip;
    s->chip = new ymfm::ym2151(*s->intf);
    s->intf->reset_state();
    s->out_pos = 0;
    s->ring_head = s->ring_tail = s->ring_count = 0;
    s->last_sample.clear();
}

void neo_ym2151_write_addr(void* ptr, int data) {
    if (!ptr) return;
    OpmState* s = static_cast<OpmState*>(ptr);
    s->chip->write_address(static_cast<uint8_t>(data & 0xff));
}

void neo_ym2151_write_data(void* ptr, int data) {
    if (!ptr) return;
    OpmState* s = static_cast<OpmState*>(ptr);
    s->chip->write_data(static_cast<uint8_t>(data & 0xff));
}

int neo_ym2151_read_status(void* ptr) {
    if (!ptr) return 0;
    OpmState* s = static_cast<OpmState*>(ptr);
    return s->chip->read_status();
}

int neo_ym2151_irq(void* ptr) {
    if (!ptr) return 0;
    OpmState* s = static_cast<OpmState*>(ptr);
    return s->intf->irq();
}

// Advance the chip by `chip_samples` native samples, decimating to the output
// rate and buffering the result.  Timer countdowns advance in lockstep.
void neo_ym2151_advance(void* ptr, int chip_samples) {
    if (!ptr) return;
    OpmState* s = static_cast<OpmState*>(ptr);
    constexpr uint64_t ONE = uint64_t(1) << 32;
    for (int i = 0; i < chip_samples; i++) {
        s->chip->generate(&s->last_sample, 1);
        s->intf->advance_clocks(s->clocks_per_sample);
        s->out_pos += s->out_step;
        if (s->out_pos >= ONE) {
            s->out_pos -= ONE;
            int32_t l = s->last_sample.data[0];
            int32_t r = s->last_sample.data[1];
            float mono = float(l + r) * 0.5f / 32768.0f;
            if (mono > 1.0f)  mono = 1.0f;
            if (mono < -1.0f) mono = -1.0f;
            push_ring(s, mono);
        }
    }
}

// Drain up to `n` output-rate samples into `buf` (mono, ±1.0).  Underflow
// yields silence.
void neo_ym2151_generate(void* ptr, float* buf, int n) {
    if (!ptr) { for (int i = 0; i < n; i++) buf[i] = 0.0f; return; }
    OpmState* s = static_cast<OpmState*>(ptr);
    for (int i = 0; i < n; i++) {
        if (s->ring_count > 0) {
            buf[i] = s->ring[s->ring_tail];
            s->ring_tail = (s->ring_tail + 1) % RING_SZ;
            s->ring_count--;
        } else {
            buf[i] = 0.0f;
        }
    }
}

int neo_ym2151_ring_count(void* ptr) {
    if (!ptr) return 0;
    return (int)static_cast<OpmState*>(ptr)->ring_count;
}

// Serialise the chip + interface (timers/IRQ) + decimation accumulator into a
// heap buffer (freed via neo_ym2151_free_state_buf).  The output ring buffer is
// deliberately NOT saved — it holds already-decimated audio, not emulation
// state, and is regenerated after a rollback (matches the YM2610 glue).
void neo_ym2151_save_state(void* ptr, uint8_t** out_buf, int* out_size) {
    if (!ptr || !out_buf || !out_size) return;
    OpmState* s = static_cast<OpmState*>(ptr);
    std::vector<uint8_t> buf;
    ymfm::ymfm_saved_state st(buf, true);
    s->chip->save_restore(st);
    s->intf->save_restore_state(st);
    uint32_t op_lo = (uint32_t)s->out_pos, op_hi = (uint32_t)(s->out_pos >> 32);
    st.save_restore(op_lo);
    st.save_restore(op_hi);
    *out_size = (int)buf.size();
    uint8_t* p = (uint8_t*)std::malloc(buf.empty() ? 1 : buf.size());
    if (!buf.empty()) std::memcpy(p, buf.data(), buf.size());
    *out_buf = p;
}

void neo_ym2151_load_state(void* ptr, const uint8_t* in_buf, int in_size) {
    if (!ptr || !in_buf || in_size <= 0) return;
    OpmState* s = static_cast<OpmState*>(ptr);
    std::vector<uint8_t> buf(in_buf, in_buf + in_size);
    ymfm::ymfm_saved_state st(buf, false);
    s->chip->save_restore(st);
    s->intf->save_restore_state(st);
    uint32_t op_lo = 0, op_hi = 0;
    st.save_restore(op_lo);
    st.save_restore(op_hi);
    s->out_pos = ((uint64_t)op_hi << 32) | op_lo;
    // Drop any buffered output samples; they are regenerated from chip state.
    s->ring_head = s->ring_tail = s->ring_count = 0;
    s->last_sample.clear();
}

void neo_ym2151_free_state_buf(uint8_t* buf) {
    std::free(buf);
}

} // extern "C"
