-- Control contracts for the pipelined bank. An independent finite convolution
-- reference checks phase-zero samples; it does not repeat the DUT's accumulator
-- implementation. Reset flushes pending work and a blocked complete output.
-- Short streams drain without extra input, while an incomplete group stays silent.
library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;
use std.env.all;

library work;
use work.rf_observatory_p.all;

entity rf_channel_bank_pipeline_tb is end entity;

architecture simulation of rf_channel_bank_pipeline_tb is
    constant STEPS : phase_steps_t := (others => (others => '0'));
    signal clock : std_logic := '0';
    signal reset : std_logic := '1';
    signal s_valid : std_logic := '0';
    signal s_ready : std_logic;
    signal s_i, s_q : signed(15 downto 0) := (others => '0');
    signal m_valid : std_logic;
    signal m_ready : std_logic := '1';
    signal m_i, m_q : iq_words_t;
    signal m_clipped : std_logic_vector(3 downto 0);
    signal m_index : unsigned(63 downto 0);
    signal accepted, emitted : natural := 0;

    function floor_divide(value : integer; divisor : positive) return integer is
        variable quotient : integer;
    begin
        quotient := value / divisor;
        if value < 0 and value rem divisor /= 0 then quotient := quotient - 1; end if;
        return quotient;
    end function;

    function input_i(index : natural) return integer is
    begin
        return ((index * 97) mod 2001) - 1000;
    end function;

    function input_q(index : natural) return integer is
    begin
        return ((index * 53) mod 1801) - 900;
    end function;
begin
    clock <= not clock after 5 ns;
    dut : entity work.rf_channel_bank
        port map(clock, reset, STEPS, s_valid, s_ready, s_i, s_q,
            m_valid, m_ready, m_i, m_q, m_clipped, m_index);

    reference : process(clock)
        type history_t is array(0 to 255) of integer;
        type expected_t is record
            index : natural;
            i, q : integer;
        end record;
        type queue_t is array(0 to 31) of expected_t;
        variable history_i, history_q : history_t := (others => 0);
        variable queue : queue_t;
        variable inputs, head, tail, cycles : natural := 0;
        variable sum_i, sum_q, source : integer;
        variable held : boolean := false;
        variable held_i, held_q : iq_words_t;
        variable held_clipped : std_logic_vector(3 downto 0);
        variable held_index : unsigned(63 downto 0);
    begin
        if rising_edge(clock) then
            cycles := cycles + 1;
            assert cycles < 100000 report "pipeline contract cycle budget exceeded" severity failure;
            if reset = '1' then
                history_i := (others => 0); history_q := (others => 0);
                inputs := 0; head := 0; tail := 0; held := false;
                accepted <= 0; emitted <= 0;
            else
                if held then
                    assert m_valid = '1' and m_i = held_i and m_q = held_q
                        and m_clipped = held_clipped and m_index = held_index
                        report "pipeline changed a blocked output" severity failure;
                end if;
                if m_valid = '1' and m_ready = '0' then
                    assert s_ready = '0'
                        report "pipeline accepted input behind a blocked output" severity failure;
                end if;
                held := m_valid = '1' and m_ready = '0';
                held_i := m_i; held_q := m_q;
                held_clipped := m_clipped; held_index := m_index;

                if s_valid = '1' and s_ready = '1' then
                    assert inputs < history_i'length report "reference history exhausted" severity failure;
                    -- Phase zero gives cos=32767, sin=0. Arithmetic shifts floor
                    -- negative products; VHDL integer division alone truncates.
                    history_i(inputs) := floor_divide(to_integer(s_i) * 32767, 32768);
                    history_q(inputs) := floor_divide(to_integer(s_q) * 32767, 32768);
                    if inputs mod 20 = 19 then
                        sum_i := 0; sum_q := 0;
                        -- CIC3/R20 is the convolution of three 20-sample boxes.
                        -- These bounded fixtures never wrap 48 bits or clip i16;
                        -- full-range numerical vectors exercise those separately.
                        for a in 0 to 19 loop
                            for b in 0 to 19 loop
                                for c in 0 to 19 loop
                                    source := inputs - a - b - c;
                                    if source >= 0 then
                                        sum_i := sum_i + history_i(source);
                                        sum_q := sum_q + history_q(source);
                                    end if;
                                end loop;
                            end loop;
                        end loop;
                        assert tail < queue'length report "reference queue exhausted" severity failure;
                        queue(tail) := (inputs, floor_divide(sum_i, 8192), floor_divide(sum_q, 8192));
                        tail := tail + 1;
                    end if;
                    inputs := inputs + 1; accepted <= inputs;
                end if;

                if m_valid = '1' and m_ready = '1' then
                    assert head < tail report "output appeared without a complete accepted group" severity failure;
                    assert m_index = to_unsigned(queue(head).index, 64)
                        report "pipeline lost accepted-sample index alignment" severity failure;
                    for channel in 0 to 3 loop
                        assert to_integer(m_i(channel)) = queue(head).i
                            and to_integer(m_q(channel)) = queue(head).q
                            and m_clipped(channel) = '0'
                            report "pipeline output disagrees with independent convolution" severity failure;
                    end loop;
                    head := head + 1; emitted <= head;
                end if;
            end if;
        end if;
    end process;

    stimulus : process
        procedure clocks(count : natural) is
        begin
            for n in 1 to count loop wait until falling_edge(clock); end loop;
        end procedure;

        procedure reset_epoch is
        begin
            s_valid <= '0'; reset <= '1'; m_ready <= '1';
            clocks(2);
            assert m_valid = '0' and s_ready = '0'
                report "reset did not flush pending pipeline work" severity failure;
            reset <= '0';
        end procedure;

        procedure send_samples(count : positive; zeros : boolean := false) is
        begin
            for n in 1 to count loop
                wait until falling_edge(clock);
                if zeros then
                    s_i <= (others => '0'); s_q <= (others => '0');
                else
                    s_i <= to_signed(input_i(accepted), 16);
                    s_q <= to_signed(input_q(accepted), 16);
                end if;
                s_valid <= '1';
                loop
                    wait until rising_edge(clock);
                    exit when s_ready = '1';
                end loop;
            end loop;
            wait until falling_edge(clock); s_valid <= '0';
        end procedure;
    begin
        clocks(2); reset_epoch;
        send_samples(19); clocks(64);
        assert accepted = 19 and emitted = 0 and m_valid = '0'
            report "partial group produced an output" severity failure;

        reset_epoch; send_samples(20); clocks(64);
        assert accepted = 20 and emitted = 1 and m_valid = '0'
            report "complete short input did not drain without further input" severity failure;

        -- Reset at every pending boundary of the nine-clock arithmetic pipeline.
        -- Output backpressure also makes the test safe if the pipeline shortens.
        -- A zero-valued new epoch makes stale nonzero arithmetic state visible.
        for boundary in 0 to 8 loop
            reset_epoch; m_ready <= '0'; send_samples(20); clocks(boundary);
            reset_epoch; clocks(64);
            assert accepted = 0 and emitted = 0 and m_valid = '0'
                report "pre-reset token escaped into a fresh epoch" severity failure;
            send_samples(20, true); clocks(64);
            assert accepted = 20 and emitted = 1
                report "reset-in-flight changed the following clean epoch" severity failure;
        end loop;

        -- The first group blocks while later accepted samples remain in flight.
        reset_epoch; m_ready <= '0'; send_samples(26); clocks(64);
        assert accepted = 26 and emitted = 0 and m_valid = '1' and s_ready = '0'
            report "failed to establish blocked output with in-flight samples" severity failure;
        s_i <= to_signed(input_i(accepted), 16);
        s_q <= to_signed(input_q(accepted), 16);
        s_valid <= '1'; clocks(17);
        assert accepted = 26 and emitted = 0
            report "blocked pipeline advanced its ingress" severity failure;
        m_ready <= '1'; wait until rising_edge(clock);
        wait until falling_edge(clock); s_valid <= '0';
        assert accepted = 27 report "resume lost the held input transfer" severity failure;
        send_samples(33); clocks(64);
        assert accepted = 60 and emitted = 3 and m_valid = '0'
            report "resume lost, repeated or stranded accepted input" severity failure;

        reset_epoch; m_ready <= '0'; send_samples(26); clocks(64);
        assert m_valid = '1' report "blocked-reset fixture has no pending output" severity failure;
        reset_epoch; clocks(64);
        assert accepted = 0 and emitted = 0 and m_valid = '0'
            report "reset retained blocked output or in-flight tokens" severity failure;
        -- Reset must clear arithmetic history as well as output-valid markers.
        send_samples(20, true); clocks(64);
        assert accepted = 20 and emitted = 1
            report "blocked reset corrupted the following clean epoch" severity failure;
        report "channel-bank pipeline drain, reset and stall contracts passed" severity note;
        finish;
    end process;
end architecture;
