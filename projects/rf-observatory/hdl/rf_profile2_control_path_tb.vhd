-- End-to-end profile-2 control-path contract. This connects the real Wishbone
-- register bank to the control/RX CDC block, drives unrelated clocks and checks
-- register-visible outcomes together with the one-start-token behavior.

library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;
use std.env.all;

library work;
use work.rf_observatory_p.all;
use work.rf_profile2_p.all;

entity rf_profile2_control_path_tb is end entity;

architecture simulation of rf_profile2_control_path_tb is
    constant WORDS_A : phase_steps_t := (
        x"00000000", x"deadbeef", x"deadbeef", x"ffffffff"
    );
    constant WORDS_B : phase_steps_t := (
        x"11111111", x"22222222", x"33333333", x"44444444"
    );
    constant WORDS_C : phase_steps_t := (
        x"55555555", x"66666666", x"77777777", x"88888888"
    );
    constant WORDS_D : phase_steps_t := (
        x"01234567", x"89abcdef", x"76543210", x"fedcba98"
    );
    constant ZERO_WORDS : phase_steps_t := (others => (others => '0'));
    constant MAX_WAIT_CYCLES : positive := 128;

    signal control_clock : std_logic := '0';
    signal control_reset : std_logic := '1';
    signal rx_clock : std_logic := '0';
    signal rx_reset : std_logic := '1';
    signal rx_enable : std_logic := '0';

    signal wb_address : std_logic_vector(31 downto 0) := (others => '0');
    signal wb_write_data : std_logic_vector(31 downto 0) := (others => '0');
    signal wb_read_data : std_logic_vector(31 downto 0);
    signal wb_write : std_logic := '0';
    signal wb_select : std_logic := '0';
    signal wb_strobe : std_logic := '0';
    signal wb_cycle : std_logic := '0';
    signal wb_ack : std_logic;

    signal rx_active_sync : std_logic;
    signal apply_valid, apply_done, apply_success : std_logic;
    signal apply_words : phase_steps_t;
    signal apply_epoch : unsigned(31 downto 0);
    signal start_pulse, start_authorized, active_valid : std_logic;
    signal active_words : phase_steps_t;
    signal active_epoch : unsigned(31 downto 0);

    signal apply_dispatches : natural := 0;
    signal apply_completions : natural := 0;
    signal start_events : natural := 0;
    signal authorized_starts : natural := 0;
begin
    control_clock <= not control_clock after 5 ns;
    rx_clock <= not rx_clock after 7 ns;

    registers : entity work.rf_profile2_wb_registers
        port map (
            clock => control_clock,
            reset => control_reset,
            wb_address => wb_address,
            wb_write_data => wb_write_data,
            wb_read_data => wb_read_data,
            wb_write => wb_write,
            wb_select => wb_select,
            wb_strobe => wb_strobe,
            wb_cycle => wb_cycle,
            wb_ack => wb_ack,
            rx_active_sync => rx_active_sync,
            apply_valid => apply_valid,
            apply_words => apply_words,
            apply_epoch => apply_epoch,
            apply_done => apply_done,
            apply_success => apply_success
        );

    cdc : entity work.rf_profile2_control_cdc
        port map (
            control_clock => control_clock,
            control_reset => control_reset,
            apply_valid => apply_valid,
            apply_words => apply_words,
            apply_epoch => apply_epoch,
            apply_done => apply_done,
            apply_success => apply_success,
            rx_active_sync => rx_active_sync,
            rx_clock => rx_clock,
            rx_reset => rx_reset,
            rx_enable => rx_enable,
            start_pulse => start_pulse,
            start_authorized => start_authorized,
            active_valid => active_valid,
            active_words => active_words,
            active_epoch => active_epoch
        );

    control_monitor : process(control_clock, control_reset)
        variable previous_ack : std_logic := '0';
        variable previous_cycle : std_logic := '0';
        variable previous_done : std_logic := '0';
        variable previous_apply_valid : std_logic := '0';
        variable request_open : boolean := false;
    begin
        if control_reset = '1' then
            previous_ack := '0';
            previous_cycle := '0';
            previous_done := '0';
            previous_apply_valid := '0';
            request_open := false;
        elsif rising_edge(control_clock) then
            assert not (previous_ack = '1' and wb_ack = '1')
                report "control-path Wishbone ACK exceeded one clock"
                severity failure;
            assert not (previous_done = '1' and apply_done = '1')
                report "control-path apply_done exceeded one clock"
                severity failure;

            if wb_cycle = '1' and previous_cycle = '0' then
                assert wb_strobe = '1' and not request_open
                    report "control-path Wishbone requests overlapped"
                    severity failure;
                request_open := true;
            end if;
            if wb_ack = '1' then
                assert request_open and wb_select = '1' and wb_cycle = '0'
                    report "control-path Wishbone ACK lacked a selected request"
                    severity failure;
                request_open := false;
            end if;
            if apply_valid = '1' and previous_apply_valid = '0' then
                apply_dispatches <= apply_dispatches + 1;
            end if;
            if apply_done = '1' then
                assert apply_valid = '1'
                    report "apply completion lacked a pending register request"
                    severity failure;
                apply_completions <= apply_completions + 1;
            end if;

            previous_ack := wb_ack;
            previous_cycle := wb_cycle;
            previous_done := apply_done;
            previous_apply_valid := apply_valid;
        end if;
    end process;

    rx_monitor : process(rx_clock, rx_reset)
        variable previous_start : std_logic := '0';
    begin
        if rx_reset = '1' then
            previous_start := '0';
        elsif rising_edge(rx_clock) then
            assert not (previous_start = '1' and start_pulse = '1')
                report "control-path start_pulse exceeded one RX clock"
                severity failure;
            assert start_authorized = '0' or start_pulse = '1'
                report "start_authorized escaped its start pulse"
                severity failure;
            if start_pulse = '1' then
                assert rx_enable = '1'
                    report "start pulse occurred while RX was disabled"
                    severity failure;
                start_events <= start_events + 1;
                if start_authorized = '1' then
                    authorized_starts <= authorized_starts + 1;
                end if;
            end if;
            previous_start := start_pulse;
        end if;
    end process;

    stimulus : process
        procedure control_cycles(count : positive) is
        begin
            for index in 1 to count loop
                wait until falling_edge(control_clock);
            end loop;
        end procedure;

        procedure rx_cycles(count : positive) is
        begin
            for index in 1 to count loop
                wait until falling_edge(rx_clock);
            end loop;
        end procedure;

        procedure transaction(
            word_address : natural;
            write_request : boolean;
            write_value : std_logic_vector(31 downto 0);
            expected_read : std_logic_vector(31 downto 0)
        ) is
            variable waited : natural := 0;
        begin
            wait until falling_edge(control_clock);
            wb_address <= std_logic_vector(to_unsigned(word_address, 32));
            wb_write_data <= write_value;
            if write_request then
                wb_write <= '1';
            else
                wb_write <= '0';
            end if;
            wb_strobe <= '1';
            wb_cycle <= '1';
            wb_select <= '0';

            wait until falling_edge(control_clock);
            -- The slave must use the request captured during cyc, not these
            -- deliberately invalid live values during its response phase.
            wb_address <= x"fffffff0";
            wb_write_data <= x"a5a55a5a";
            wb_write <= not wb_write;
            wb_cycle <= '0';
            wb_select <= '1';

            loop
                wait until falling_edge(control_clock);
                exit when wb_ack = '1';
                waited := waited + 1;
                assert waited < MAX_WAIT_CYCLES
                    report "control-path Wishbone request did not terminate"
                    severity failure;
            end loop;
            if not write_request then
                assert wb_read_data = expected_read
                    report "control-path register read differs" severity failure;
            end if;

            wait until falling_edge(control_clock);
            assert wb_ack = '0'
                report "control-path Wishbone ACK did not clear" severity failure;
            wb_select <= '0';
            wb_strobe <= '0';
            wb_write <= '0';
            wait until falling_edge(control_clock);
        end procedure;

        procedure write_word(
            word_address : natural;
            value : std_logic_vector(31 downto 0)
        ) is
        begin
            transaction(word_address, true, value, (others => '0'));
        end procedure;

        procedure read_word(
            word_address : natural;
            expected : std_logic_vector(31 downto 0)
        ) is
        begin
            transaction(word_address, false, (others => '0'), expected);
        end procedure;

        procedure write_bank(words : phase_steps_t) is
        begin
            for channel in 0 to 3 loop
                write_word(P2_ADDR_SHADOW_FIRST + channel,
                    std_logic_vector(words(channel)));
            end loop;
        end procedure;

        procedure read_applied_bank(
            epoch : natural;
            words : phase_steps_t
        ) is
        begin
            read_word(P2_ADDR_APPLIED_EPOCH,
                std_logic_vector(to_unsigned(epoch, 32)));
            for channel in 0 to 3 loop
                read_word(P2_ADDR_APPLIED_FIRST + channel,
                    std_logic_vector(words(channel)));
            end loop;
        end procedure;

        procedure wait_apply_completion(expected_success : std_logic) is
            variable waited : natural := 0;
        begin
            loop
                wait until falling_edge(control_clock);
                exit when apply_done = '1';
                waited := waited + 1;
                assert waited < MAX_WAIT_CYCLES
                    report "control-path apply did not complete" severity failure;
            end loop;
            assert apply_success = expected_success
                report "control-path apply result differs" severity failure;
            wait until falling_edge(control_clock);
            assert apply_done = '0' and apply_valid = '0'
                report "apply result did not release after one clock"
                severity failure;
        end procedure;

        procedure wait_rx_active(expected : std_logic) is
            variable waited : natural := 0;
        begin
            loop
                wait until falling_edge(control_clock);
                exit when rx_active_sync = expected;
                waited := waited + 1;
                assert waited < MAX_WAIT_CYCLES
                    report "synchronized RX-active level did not arrive"
                    severity failure;
            end loop;
        end procedure;

        procedure wait_start(
            expected_authorized : std_logic;
            expected_words : phase_steps_t;
            expected_epoch : natural
        ) is
            variable waited : natural := 0;
        begin
            loop
                wait until falling_edge(rx_clock);
                exit when start_pulse = '1';
                waited := waited + 1;
                assert waited < MAX_WAIT_CYCLES
                    report "RX start event did not arrive" severity failure;
            end loop;
            assert start_authorized = expected_authorized
                report "RX start authorization differs" severity failure;
            if expected_authorized = '1' then
                assert active_valid = '1' and active_words = expected_words
                    and active_epoch = to_unsigned(expected_epoch, 32)
                    report "authorized start did not publish its atomic bank"
                    severity failure;
            else
                assert active_valid = '0' and active_words = ZERO_WORDS
                    and active_epoch = 0
                    report "unauthorized start exposed stale active state"
                    severity failure;
            end if;
            wait until falling_edge(rx_clock);
            assert start_pulse = '0' and start_authorized = '0'
                report "RX start result did not clear after one clock"
                severity failure;
        end procedure;

        procedure wait_active_clear is
            variable waited : natural := 0;
        begin
            loop
                wait until falling_edge(rx_clock);
                exit when active_valid = '0' and active_words = ZERO_WORDS
                    and active_epoch = 0;
                waited := waited + 1;
                assert waited < MAX_WAIT_CYCLES
                    report "stopping RX did not clear active state"
                    severity failure;
            end loop;
        end procedure;

        variable before_dispatches : natural;
        variable before_completions : natural;
        variable before_starts : natural;
        variable before_authorized : natural;
    begin
        control_cycles(3);
        rx_cycles(3);
        assert wb_ack = '0' and apply_valid = '0' and apply_done = '0'
            and start_pulse = '0' and active_valid = '0'
            report "control path was not idle during startup reset"
            severity failure;

        -- Release the two domains on unrelated clock phases.
        rx_reset <= '0';
        rx_cycles(2);
        control_reset <= '0';
        control_cycles(5);
        rx_cycles(5);
        read_word(P2_ADDR_STATUS,
            p2_status('0', '0', '0', '0', P2_REASON_NONE, "0000"));

        -- Apply A successfully, then replace its unused token with B.
        write_bank(WORDS_A);
        write_word(P2_ADDR_COMMIT, P2_COMMIT_MAGIC);
        wait_apply_completion('1');
        read_word(P2_ADDR_STATUS,
            p2_status('0', '1', '1', '0', P2_REASON_NONE, "0000"));
        read_word(P2_ADDR_OUTCOME_EPOCH, x"00000001");
        read_applied_bank(1, WORDS_A);

        write_bank(WORDS_B);
        write_word(P2_ADDR_COMMIT, P2_COMMIT_MAGIC);
        wait_apply_completion('1');
        read_word(P2_ADDR_STATUS,
            p2_status('0', '1', '1', '0', P2_REASON_NONE, "0000"));
        read_word(P2_ADDR_OUTCOME_EPOCH, x"00000002");
        read_applied_bank(2, WORDS_B);

        -- The one saturated token starts exactly B; holding enable high must not
        -- create another start or alter the active bank.
        before_starts := start_events;
        before_authorized := authorized_starts;
        wait until falling_edge(rx_clock);
        rx_enable <= '1';
        wait_start('1', WORDS_B, 2);
        rx_cycles(12);
        assert start_events = before_starts + 1
            and authorized_starts = before_authorized + 1
            and active_valid = '1' and active_words = WORDS_B
            and active_epoch = 2
            report "held-high RX enable repeated or changed the B start"
            severity failure;
        wait_rx_active('1');

        -- Writes remain legal while active, but COMMIT must reject locally and
        -- must not dispatch a CDC apply request.
        write_bank(WORDS_C);
        before_dispatches := apply_dispatches;
        before_completions := apply_completions;
        write_word(P2_ADDR_COMMIT, P2_COMMIT_MAGIC);
        control_cycles(8);
        assert apply_valid = '0' and apply_dispatches = before_dispatches
            and apply_completions = before_completions
            report "RX-active rejection dispatched an apply request"
            severity failure;
        read_word(P2_ADDR_STATUS,
            p2_status('0', '1', '0', '1', P2_REASON_RX_ACTIVE, "1111"));
        read_word(P2_ADDR_OUTCOME_EPOCH, x"00000003");
        read_applied_bank(2, WORDS_B);
        assert start_events = before_starts + 1
            and active_valid = '1' and active_words = WORDS_B
            report "active register traffic disturbed the running B epoch"
            severity failure;

        -- Stop clears active state. A restart without another successful commit
        -- emits one unauthorized start and never resurrects B.
        wait until falling_edge(rx_clock);
        rx_enable <= '0';
        wait_active_clear;
        wait_rx_active('0');
        before_starts := start_events;
        before_authorized := authorized_starts;
        wait until falling_edge(rx_clock);
        rx_enable <= '1';
        wait_start('0', ZERO_WORDS, 0);
        rx_cycles(8);
        assert start_events = before_starts + 1
            and authorized_starts = before_authorized
            and active_valid = '0' and active_words = ZERO_WORDS
            and active_epoch = 0
            report "restart without commit was not a single invalid start"
            severity failure;
        wait until falling_edge(rx_clock);
        rx_enable <= '0';
        wait_active_clear;
        wait_rx_active('0');

        -- The rejected active commit preserved C and its full mask. Dispatch it
        -- while stopped, then assert a skewed common reset before completion.
        write_word(P2_ADDR_COMMIT, P2_COMMIT_MAGIC);
        assert apply_valid = '1'
            report "pending-reset fixture did not dispatch C" severity failure;
        before_completions := apply_completions;
        before_starts := start_events;
        control_reset <= '1';
        control_cycles(1);
        rx_reset <= '1';
        control_cycles(3);
        rx_cycles(3);
        assert wb_ack = '0' and apply_valid = '0' and apply_done = '0'
            and start_pulse = '0' and active_valid = '0'
            report "common reset did not cancel the pending control path"
            severity failure;

        -- Release control first this time and allow every synchronizer to drain.
        control_reset <= '0';
        control_cycles(2);
        rx_reset <= '0';
        control_cycles(12);
        rx_cycles(12);
        assert apply_completions = before_completions
            and start_events = before_starts and apply_valid = '0'
            and active_valid = '0'
            report "pending reset produced a ghost completion or start"
            severity failure;
        read_word(P2_ADDR_STATUS,
            p2_status('0', '0', '0', '0', P2_REASON_NONE, "0000"));
        read_applied_bank(0, ZERO_WORDS);

        -- Arm A without starting it, then assert the common reset in the opposite
        -- order. The unused token and successful outcome must both disappear.
        write_bank(WORDS_A);
        write_word(P2_ADDR_COMMIT, P2_COMMIT_MAGIC);
        wait_apply_completion('1');
        read_applied_bank(1, WORDS_A);
        before_completions := apply_completions;
        before_starts := start_events;
        rx_reset <= '1';
        rx_cycles(1);
        control_reset <= '1';
        control_cycles(3);
        rx_cycles(3);
        rx_reset <= '0';
        rx_cycles(2);
        control_reset <= '0';
        control_cycles(12);
        rx_cycles(12);
        assert apply_completions = before_completions
            and start_events = before_starts and apply_valid = '0'
            and active_valid = '0' and active_words = ZERO_WORDS
            report "armed reset produced a ghost completion or start"
            severity failure;
        read_word(P2_ADDR_STATUS,
            p2_status('0', '0', '0', '0', P2_REASON_NONE, "0000"));
        read_applied_bank(0, ZERO_WORDS);

        -- A clean post-reset D request restarts epoch numbering and yields one
        -- authorized start, proving that neither skewed reset stranded phases.
        write_bank(WORDS_D);
        write_word(P2_ADDR_COMMIT, P2_COMMIT_MAGIC);
        wait_apply_completion('1');
        read_word(P2_ADDR_STATUS,
            p2_status('0', '1', '1', '0', P2_REASON_NONE, "0000"));
        read_word(P2_ADDR_OUTCOME_EPOCH, x"00000001");
        read_applied_bank(1, WORDS_D);
        before_starts := start_events;
        before_authorized := authorized_starts;
        wait until falling_edge(rx_clock);
        rx_enable <= '1';
        wait_start('1', WORDS_D, 1);
        rx_cycles(4);
        assert start_events = before_starts + 1
            and authorized_starts = before_authorized + 1
            and active_valid = '1' and active_words = WORDS_D
            and active_epoch = 1
            report "clean post-reset D start differs" severity failure;

        report "profile-2 integrated control-path contract passed" severity note;
        finish;
    end process;

    watchdog : process
    begin
        wait for 100 us;
        assert false report "profile-2 control-path bench timed out"
            severity failure;
    end process;
end architecture;
