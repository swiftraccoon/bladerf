-- Independent profile-2 transport and lifecycle contracts. Literal wire slots
-- below deliberately do not come from DUT constants or an encoder shared with
-- the implementation. The fixtures use zero input samples so DATA payloads are
-- independently known while the real channel bank still establishes admission
-- and accepted-source index behavior.

library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;
use std.env.all;

library work;
use work.rf_observatory_p.all;

entity rf_profile2_framer_contract_tb is
end entity;

architecture simulation of rf_profile2_framer_contract_tb is
    constant GOLDEN_WORDS : phase_steps_t := (
        x"01234567", x"89abcdef", x"80000001", x"deadbeef"
    );
    constant ZERO_WORDS : phase_steps_t := (others => (others => '0'));
    constant GOLDEN_EPOCH : unsigned(31 downto 0) := x"12345678";

    signal clock : std_logic := '0';
    signal reset : std_logic := '1';
    signal start_pulse : std_logic := '0';
    signal start_authorized : std_logic := '0';
    signal active_valid : std_logic := '0';
    signal active_words : phase_steps_t := ZERO_WORDS;
    signal active_epoch : unsigned(31 downto 0) := (others => '0');
    signal s_valid : std_logic := '0';
    signal s_ready : std_logic;
    signal s_i, s_q : signed(15 downto 0) := (others => '0');
    signal source_rx2 : std_logic := '0';
    signal configuration_error : std_logic := '0';
    signal transport_loss : std_logic := '0';
    signal m_valid : std_logic;
    signal m_ready : std_logic := '0';
    signal m_i, m_q : signed(15 downto 0);
begin
    clock <= not clock after 5 ns;

    dut : entity work.rf_profile2_framer
        port map (
            clock => clock,
            reset => reset,
            start_pulse => start_pulse,
            start_authorized => start_authorized,
            active_valid => active_valid,
            active_words => active_words,
            active_epoch => active_epoch,
            s_valid => s_valid,
            s_ready => s_ready,
            s_i => s_i,
            s_q => s_q,
            source_rx2 => source_rx2,
            configuration_error => configuration_error,
            transport_loss => transport_loss,
            m_valid => m_valid,
            m_ready => m_ready,
            m_i => m_i,
            m_q => m_q
        );

    stimulus : process
        procedure wait_output(constant label_text : in string) is
        begin
            for cycle in 0 to 255 loop
                if m_valid = '1' then
                    return;
                end if;
                wait until falling_edge(clock);
            end loop;
            assert false report label_text & ": output wait exceeded bound"
                severity failure;
        end procedure;

        procedure expect_slot(
            constant expected_i : in std_logic_vector(15 downto 0);
            constant expected_q : in std_logic_vector(15 downto 0);
            constant stalls : in natural;
            constant config_slot : in boolean;
            constant label_text : in string
        ) is
            variable held_i, held_q : signed(15 downto 0);
        begin
            m_ready <= '0';
            wait_output(label_text);
            assert std_logic_vector(m_i) = expected_i
                and std_logic_vector(m_q) = expected_q
                report label_text & ": literal slot mismatch" severity failure;
            if config_slot then
                assert s_ready = '0'
                    report label_text & ": CONFIG admitted an ADC sample"
                    severity failure;
            end if;
            held_i := m_i;
            held_q := m_q;
            for cycle in 1 to stalls loop
                wait until rising_edge(clock);
                wait until falling_edge(clock);
                assert m_valid = '1' and m_i = held_i and m_q = held_q
                    report label_text & ": blocked output changed" severity failure;
                if config_slot then
                    assert s_ready = '0'
                        report label_text & ": stalled CONFIG admitted an ADC sample"
                        severity failure;
                end if;
            end loop;
            m_ready <= '1';
            wait until rising_edge(clock);
            if config_slot then
                assert s_ready = '0'
                    report label_text & ": CONFIG handshake admitted an ADC sample"
                    severity failure;
            end if;
            wait until falling_edge(clock);
            m_ready <= '0';
        end procedure;

        procedure pulse_start(
            constant authorized : in std_logic;
            constant valid : in std_logic;
            constant words : in phase_steps_t;
            constant epoch : in unsigned(31 downto 0);
            constant rx2 : in std_logic
        ) is
        begin
            wait until falling_edge(clock);
            start_authorized <= authorized;
            active_valid <= valid;
            active_words <= words;
            active_epoch <= epoch;
            source_rx2 <= rx2;
            start_pulse <= '1';
            wait until rising_edge(clock);
            wait until falling_edge(clock);
            start_pulse <= '0';
        end procedure;

        procedure send_zero_samples(constant count : in positive) is
            variable waited : natural;
        begin
            s_i <= (others => '0');
            s_q <= (others => '0');
            s_valid <= '1';
            for sample in 1 to count loop
                waited := 0;
                loop
                    wait until rising_edge(clock);
                    exit when s_ready = '1';
                    waited := waited + 1;
                    assert waited < 256
                        report "sample-admission wait exceeded bound" severity failure;
                end loop;
                wait until falling_edge(clock);
            end loop;
            s_valid <= '0';
        end procedure;

        procedure expect_valid_config_through_slot6(
            constant stall_each_slot : in boolean
        ) is
            variable delay : natural := 0;
        begin
            if stall_each_slot then delay := 1; end if;
            expect_slot(x"5246", x"4f42", delay, true, "CONFIG slot 0");
            expect_slot(x"2038", x"0002", delay, true, "CONFIG slot 1");
            expect_slot(x"5678", x"1234", delay, true, "CONFIG slot 2");
            expect_slot(x"2d00", x"0131", delay, true, "CONFIG slot 3");
            expect_slot(x"4567", x"0123", delay, true, "CONFIG slot 4");
            expect_slot(x"cdef", x"89ab", delay, true, "CONFIG slot 5");
            expect_slot(x"0001", x"8000", delay, true, "CONFIG slot 6");
        end procedure;

        procedure expect_valid_config(constant stall_each_slot : in boolean) is
            variable delay : natural := 0;
        begin
            expect_valid_config_through_slot6(stall_each_slot);
            if stall_each_slot then delay := 3; end if;
            expect_slot(x"beef", x"dead", delay, true, "CONFIG slot 7");
        end procedure;

        procedure expect_invalid_config(constant stall_each_slot : in boolean) is
            variable delay : natural := 0;
        begin
            if stall_each_slot then delay := 1; end if;
            expect_slot(x"5246", x"4f42", delay, true,
                "invalid CONFIG slot 0");
            expect_slot(x"2050", x"0002", delay, true,
                "invalid CONFIG slot 1");
            expect_slot(x"0000", x"0000", delay, true,
                "invalid CONFIG slot 2");
            expect_slot(x"2d00", x"0131", delay, true,
                "invalid CONFIG slot 3");
            for slot in 4 to 7 loop
                expect_slot(x"0000", x"0000", delay, true,
                    "invalid CONFIG zero payload");
            end loop;
        end procedure;

        procedure expect_zero_data(
            constant flags : in std_logic_vector(15 downto 0);
            constant index_low : in std_logic_vector(15 downto 0);
            constant stall_each_slot : in boolean;
            constant label_text : in string
        ) is
            variable delay : natural := 0;
        begin
            if stall_each_slot then delay := 1; end if;
            expect_slot(x"5246", x"4f42", delay, false,
                label_text & " slot 0");
            expect_slot(flags, x"0002", delay, false,
                label_text & " slot 1");
            expect_slot(index_low, x"0000", delay, false,
                label_text & " slot 2");
            expect_slot(x"0000", x"0000", delay, false,
                label_text & " slot 3");
            for slot in 4 to 7 loop
                expect_slot(x"0000", x"0000", delay, false,
                    label_text & " zero sample");
            end loop;
        end procedure;

        procedure silence(
            constant cycles : in positive;
            constant require_ingress_closed : in boolean;
            constant label_text : in string
        ) is
        begin
            m_ready <= '0';
            for cycle in 1 to cycles loop
                wait until rising_edge(clock);
                wait until falling_edge(clock);
                assert m_valid = '0'
                    report label_text & ": stale or duplicate output appeared"
                    severity failure;
                if require_ingress_closed then
                    assert s_ready = '0'
                        report label_text & ": ingress unexpectedly reopened"
                        severity failure;
                end if;
            end loop;
        end procedure;

        procedure reset_and_check(constant label_text : in string) is
        begin
            start_pulse <= '0';
            s_valid <= '0';
            m_ready <= '0';
            reset <= '1';
            wait until rising_edge(clock);
            wait until falling_edge(clock);
            assert m_valid = '0' and s_ready = '0'
                report label_text & ": reset left transport state visible"
                severity failure;
            wait until rising_edge(clock);
            wait until falling_edge(clock);
            reset <= '0';
            silence(6, true, label_text & " recovery");
        end procedure;
    begin
        -- Establish a clean reset image before the first start.
        for cycle in 1 to 3 loop
            wait until falling_edge(clock);
        end loop;
        reset <= '0';
        silence(3, true, "initial idle");

        -- Samples physically present during CONFIG are discarded. Every CONFIG
        -- slot, especially slot 7, is stalled and must remain stable with
        -- ingress closed. Exactly 20 later handshakes produce index 19.
        s_i <= to_signed(1234, 16);
        s_q <= to_signed(-2345, 16);
        s_valid <= '1';
        pulse_start('1', '1', GOLDEN_WORDS, GOLDEN_EPOCH, '1');
        expect_valid_config(true);
        s_valid <= '0';
        send_zero_samples(20);
        expect_zero_data(x"2028", x"0013", true, "first DATA");

        -- External transport loss is sticky, but cannot alter an already
        -- serialized record. The next two complete records both report it.
        transport_loss <= '1';
        wait until rising_edge(clock);
        wait until falling_edge(clock);
        transport_loss <= '0';
        send_zero_samples(20);
        expect_zero_data(x"20a8", x"0027", false, "loss DATA");
        send_zero_samples(20);
        expect_zero_data(x"20a8", x"003b", false, "sticky-loss DATA");

        -- The explicit configuration diagnostic is independently sticky.
        configuration_error <= '1';
        wait until rising_edge(clock);
        wait until falling_edge(clock);
        configuration_error <= '0';
        send_zero_samples(20);
        expect_zero_data(x"20e8", x"004f", false, "config-error DATA");
        send_zero_samples(20);
        expect_zero_data(x"20e8", x"0063", false,
            "sticky-config-error DATA");

        -- Cut reset while CONFIG slot 7 is blocked. The final slot and any
        -- implied DATA lifetime must not reappear after reset.
        reset_and_check("pre-CONFIG-cut reset");
        s_valid <= '1';
        pulse_start('1', '1', GOLDEN_WORDS, GOLDEN_EPOCH, '1');
        expect_valid_config_through_slot6(true);
        m_ready <= '0';
        wait_output("CONFIG-cut slot 7");
        assert std_logic_vector(m_i) = x"beef"
            and std_logic_vector(m_q) = x"dead" and s_ready = '0'
            report "CONFIG reset fixture did not reach slot 7"
            severity failure;
        wait until rising_edge(clock);
        wait until falling_edge(clock);
        assert m_valid = '1' and std_logic_vector(m_i) = x"beef"
            and std_logic_vector(m_q) = x"dead" and s_ready = '0'
            report "CONFIG slot 7 reset fixture was not stably blocked"
            severity failure;
        reset_and_check("reset during blocked CONFIG slot 7");

        -- Accept slot 7, observe the externally visible CONFIG-to-DATA boundary,
        -- then reset before a DATA-domain rising edge can admit a sample.
        s_valid <= '1';
        pulse_start('1', '1', GOLDEN_WORDS, GOLDEN_EPOCH, '1');
        expect_valid_config_through_slot6(false);
        m_ready <= '0';
        wait_output("CONFIG boundary slot 7");
        assert std_logic_vector(m_i) = x"beef"
            and std_logic_vector(m_q) = x"dead" and s_ready = '0'
            report "CONFIG boundary fixture did not reach slot 7"
            severity failure;
        m_ready <= '1';
        wait until rising_edge(clock);
        wait until falling_edge(clock);
        m_ready <= '0';
        assert m_valid = '0' and s_ready = '1'
            report "CONFIG slot 7 did not enter the DATA admission boundary"
            severity failure;
        reset_and_check("reset at CONFIG-to-DATA boundary");

        -- Recover cleanly from both boundary cuts. A later source change sets
        -- the configuration flag while retaining start-time RX2, then sticks.
        pulse_start('1', '1', GOLDEN_WORDS, GOLDEN_EPOCH, '1');
        expect_valid_config(false);
        send_zero_samples(20);
        expect_zero_data(x"2028", x"0013", false,
            "boundary-reset recovery DATA");
        source_rx2 <= '0';
        wait until rising_edge(clock);
        wait until falling_edge(clock);
        send_zero_samples(20);
        expect_zero_data(x"2068", x"0027", false,
            "source-change DATA");
        source_rx2 <= '1';
        send_zero_samples(20);
        expect_slot(x"5246", x"4f42", 0, false,
            "sticky-source DATA slot 0");
        expect_slot(x"2068", x"0002", 0, false,
            "sticky-source DATA slot 1");
        expect_slot(x"003b", x"0000", 0, false,
            "sticky-source DATA slot 2");
        expect_slot(x"0000", x"0000", 0, false,
            "sticky-source DATA slot 3");
        m_ready <= '0';
        wait_output("DATA reset cut");
        assert std_logic_vector(m_i) = x"0000"
            and std_logic_vector(m_q) = x"0000"
            report "DATA reset fixture did not reach sample payload"
            severity failure;
        wait until rising_edge(clock);
        wait until falling_edge(clock);
        assert m_valid = '1' and std_logic_vector(m_i) = x"0000"
            and std_logic_vector(m_q) = x"0000"
            report "DATA reset fixture was not stably blocked"
            severity failure;
        reset_and_check("reset during DATA");

        -- A new unauthorized start while an epoch is active cannot replace its
        -- tuple or insert a second CONFIG. It is fail-visible in sticky DATA
        -- configuration status, and reset recovery restarts indexing at 19.
        pulse_start('1', '1', GOLDEN_WORDS, GOLDEN_EPOCH, '1');
        expect_valid_config(false);
        pulse_start('0', '0', ZERO_WORDS, to_unsigned(0, 32), '1');
        send_zero_samples(20);
        expect_zero_data(x"2068", x"0013", false,
            "active-start violation DATA");
        send_zero_samples(20);
        expect_zero_data(x"2068", x"0027", false,
            "sticky active-start violation DATA");
        reset_and_check("post-active-start reset");

        -- Authorization alone is insufficient: epoch zero is the reset/no-image
        -- value and therefore emits the canonical zero-tuple diagnostic only.
        pulse_start('1', '1', GOLDEN_WORDS, to_unsigned(0, 32), '0');
        expect_invalid_config(false);
        silence(16, true, "authorized zero-epoch start");
        reset_and_check("post-zero-epoch reset");

        -- A transport failure already present at the start boundary likewise
        -- invalidates an otherwise authorized tuple. Clear the live input after
        -- that edge to prove the start-time sample itself controls the outcome.
        transport_loss <= '1';
        pulse_start('1', '1', GOLDEN_WORDS, GOLDEN_EPOCH, '0');
        transport_loss <= '0';
        expect_invalid_config(false);
        silence(16, true, "start-time transport-loss diagnostic");
        reset_and_check("post-start-loss reset");

        -- Transport loss arising after a valid tuple was snapshotted cannot
        -- rewrite CONFIG, but it fail-closes the lifetime before DATA admission.
        pulse_start('1', '1', GOLDEN_WORDS, GOLDEN_EPOCH, '1');
        expect_slot(x"5246", x"4f42", 0, true,
            "live-loss CONFIG slot 0");
        expect_slot(x"2038", x"0002", 0, true,
            "live-loss CONFIG slot 1");
        expect_slot(x"5678", x"1234", 0, true,
            "live-loss CONFIG slot 2");
        expect_slot(x"2d00", x"0131", 0, true,
            "live-loss CONFIG slot 3");
        transport_loss <= '1';
        wait until rising_edge(clock);
        wait until falling_edge(clock);
        transport_loss <= '0';
        expect_slot(x"4567", x"0123", 0, true,
            "live-loss CONFIG slot 4");
        expect_slot(x"cdef", x"89ab", 0, true,
            "live-loss CONFIG slot 5");
        expect_slot(x"0001", x"8000", 0, true,
            "live-loss CONFIG slot 6");
        expect_slot(x"beef", x"dead", 0, true,
            "live-loss CONFIG slot 7");
        silence(16, true, "CONFIG-time transport-loss fail-close");
        reset_and_check("post-CONFIG-loss reset");

        -- Exercise the slot-7 predicate on the exact handshake edge. A second
        -- start, explicit configuration error and source change arrive together;
        -- the already-held CONFIG stays atomic and no DATA lifetime may open.
        pulse_start('1', '1', GOLDEN_WORDS, GOLDEN_EPOCH, '1');
        expect_valid_config_through_slot6(false);
        m_ready <= '0';
        wait_output("same-edge violation CONFIG slot 7");
        assert std_logic_vector(m_i) = x"beef"
            and std_logic_vector(m_q) = x"dead" and s_ready = '0'
            report "same-edge CONFIG violation fixture missed slot 7"
            severity failure;
        start_authorized <= '0';
        active_valid <= '0';
        active_words <= ZERO_WORDS;
        active_epoch <= (others => '0');
        start_pulse <= '1';
        configuration_error <= '1';
        source_rx2 <= '0';
        m_ready <= '1';
        wait until rising_edge(clock);
        wait until falling_edge(clock);
        start_pulse <= '0';
        configuration_error <= '0';
        m_ready <= '0';
        silence(16, true, "same-edge CONFIG slot-7 fail-close");
        reset_and_check("post-slot-7-violation reset");

        -- rf_channel_bank documents that output valid follows the twentieth
        -- admitted sample by nine advancing clocks. Assert an error only on the
        -- following framer snapshot edge: the DATA flags must include that live
        -- error even though the registered sticky bit was clear before the edge.
        pulse_start('1', '1', GOLDEN_WORDS, GOLDEN_EPOCH, '1');
        expect_valid_config(false);
        send_zero_samples(20);
        for latency in 1 to 9 loop
            wait until rising_edge(clock);
            wait until falling_edge(clock);
            assert m_valid = '0'
                report "DATA appeared before documented bank latency"
                severity failure;
        end loop;
        configuration_error <= '1';
        wait until rising_edge(clock);
        wait until falling_edge(clock);
        configuration_error <= '0';
        expect_zero_data(x"2068", x"0013", false,
            "same-edge DATA snapshot error");
        reset_and_check("post-snapshot-edge reset");

        -- An uncommitted start has one canonical error CONFIG, admits no input,
        -- and never produces DATA. Its zero tuple and RX1 source are literal.
        s_valid <= '1';
        pulse_start('0', '0', ZERO_WORDS, to_unsigned(0, 32), '0');
        expect_invalid_config(true);
        for cycle in 1 to 32 loop
            wait until rising_edge(clock);
            wait until falling_edge(clock);
            assert m_valid = '0' and s_ready = '0'
                report "invalid start emitted DATA or admitted a sample"
                severity failure;
        end loop;
        s_valid <= '0';

        report "profile-2 framer wire and lifecycle contracts passed"
            severity note;
        finish;
    end process;

    watchdog : process
    begin
        wait for 5 ms;
        assert false report "profile-2 framer contract watchdog expired"
            severity failure;
    end process;
end architecture;
