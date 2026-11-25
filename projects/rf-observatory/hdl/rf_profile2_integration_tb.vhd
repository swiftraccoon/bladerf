-- End-to-end profile-2 integration contract. The bench commits through the
-- terminating Wishbone slave, crosses unrelated clocks, survives a normal
-- stream reset, and proves CONFIG persistence at the 64-bit FIFO write
-- boundary under write-side-full/reset-release backpressure.

library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;
use std.env.all;

library work;
use work.rf_observatory_p.all;
use work.rf_profile2_p.all;

entity rf_profile2_integration_tb is end entity;

architecture simulation of rf_profile2_integration_tb is
    constant WORDS_A : phase_steps_t := (
        x"01234567", x"89abcdef", x"80000001", x"deadbeef"
    );
    constant WORDS_B : phase_steps_t := (
        x"11112222", x"33334444", x"55556666", x"77778888"
    );
    constant WORDS_C : phase_steps_t := (
        x"aaaabbbb", x"ccccdddd", x"eeeeffff", x"12345678"
    );
    constant MAX_WAIT_CYCLES : positive := 256;

    type fifo_words_t is array (0 to 31) of std_logic_vector(63 downto 0);

    signal control_clock : std_logic := '0';
    signal control_reset : std_logic := '1';
    signal rx_clock : std_logic := '0';
    signal profile_rx_reset : std_logic := '1';
    signal stream_reset : std_logic := '1';
    signal rx_enable : std_logic := '0';

    signal wb_address : std_logic_vector(31 downto 0) := (others => '0');
    signal wb_write_data : std_logic_vector(31 downto 0) := (others => '0');
    signal wb_read_data : std_logic_vector(31 downto 0);
    signal wb_write : std_logic := '0';
    signal wb_select : std_logic := '0';
    signal wb_strobe : std_logic := '0';
    signal wb_cycle : std_logic := '0';
    signal wb_ack : std_logic;

    signal s_valid : std_logic := '0';
    signal s_ready : std_logic;
    signal s_i, s_q : signed(15 downto 0) := (others => '0');
    signal source_rx2 : std_logic := '0';
    signal configuration_error : std_logic := '0';
    signal transport_loss : std_logic := '0';

    signal fifo_full : std_logic := '1';
    signal fifo_write : std_logic;
    signal fifo_data : std_logic_vector(63 downto 0);
    signal fifo_words : fifo_words_t := (others => (others => '0'));
    signal write_count : natural range 0 to fifo_words'length := 0;
begin
    control_clock <= not control_clock after 5 ns;
    rx_clock <= not rx_clock after 6.5 ns;

    dut : entity work.rf_profile2_integration
        port map (
            control_clock => control_clock,
            control_reset => control_reset,
            wb_address => wb_address,
            wb_write_data => wb_write_data,
            wb_read_data => wb_read_data,
            wb_write => wb_write,
            wb_select => wb_select,
            wb_strobe => wb_strobe,
            wb_cycle => wb_cycle,
            wb_ack => wb_ack,
            rx_clock => rx_clock,
            profile_rx_reset => profile_rx_reset,
            stream_reset => stream_reset,
            rx_enable => rx_enable,
            s_valid => s_valid,
            s_ready => s_ready,
            s_i => s_i,
            s_q => s_q,
            source_rx2 => source_rx2,
            configuration_error => configuration_error,
            transport_loss => transport_loss,
            fifo_full => fifo_full,
            fifo_write => fifo_write,
            fifo_data => fifo_data
        );

    fifo_monitor : process(rx_clock)
    begin
        if rising_edge(rx_clock) then
            assert fifo_write = '0' or fifo_full = '0'
                report "integration wrote while the RX FIFO was full/resetting"
                severity failure;
            if fifo_write = '1' then
                assert write_count < fifo_words'length
                    report "integration FIFO monitor overflow" severity failure;
                fifo_words(write_count) <= fifo_data;
                -- CONFIG slot 7 and its FIFO write are one boundary. Input
                -- admission must still be closed on that same edge.
                if write_count = 3 or write_count = 11 or write_count = 15 then
                    assert s_ready = '0'
                        report "ADC input opened before CONFIG persisted"
                        severity failure;
                end if;
                write_count <= write_count + 1;
            end if;
        end if;
    end process;

    stimulus : process
        procedure control_cycles(constant count : in positive) is
        begin
            for cycle in 1 to count loop
                wait until falling_edge(control_clock);
            end loop;
        end procedure;

        procedure rx_cycles(constant count : in positive) is
        begin
            for cycle in 1 to count loop
                wait until falling_edge(rx_clock);
            end loop;
        end procedure;

        procedure transaction(
            constant word_address : in natural;
            constant write_request : in boolean;
            constant write_value : in std_logic_vector(31 downto 0);
            constant expected_read : in std_logic_vector(31 downto 0)
        ) is
            variable waited : natural := 0;
        begin
            wait until falling_edge(control_clock);
            wb_address <= std_logic_vector(to_unsigned(word_address, 32));
            wb_write_data <= write_value;
            if write_request then wb_write <= '1'; else wb_write <= '0'; end if;
            wb_strobe <= '1';
            wb_cycle <= '1';
            wb_select <= '0';

            wait until falling_edge(control_clock);
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
                    report "integration Wishbone request did not terminate"
                    severity failure;
            end loop;
            if not write_request then
                assert wb_read_data = expected_read
                    report "integration Wishbone read mismatch" severity failure;
            end if;

            wait until falling_edge(control_clock);
            assert wb_ack = '0'
                report "integration Wishbone ACK exceeded one clock"
                severity failure;
            wb_select <= '0';
            wb_strobe <= '0';
            wb_write <= '0';
            wait until falling_edge(control_clock);
        end procedure;

        procedure write_word(
            constant word_address : in natural;
            constant value : in std_logic_vector(31 downto 0)
        ) is
        begin
            transaction(word_address, true, value, (others => '0'));
        end procedure;

        procedure read_word(
            constant word_address : in natural;
            constant expected : in std_logic_vector(31 downto 0)
        ) is
        begin
            transaction(word_address, false, (others => '0'), expected);
        end procedure;

        procedure commit_bank(
            constant words : in phase_steps_t;
            constant expected_epoch : in natural
        ) is
        begin
            for channel in words'range loop
                write_word(P2_ADDR_SHADOW_FIRST + channel,
                    std_logic_vector(words(channel)));
            end loop;
            write_word(P2_ADDR_COMMIT, P2_COMMIT_MAGIC);
            control_cycles(40);
            read_word(P2_ADDR_STATUS, x"60000000");
            read_word(P2_ADDR_APPLIED_EPOCH,
                std_logic_vector(to_unsigned(expected_epoch, 32)));
        end procedure;

        procedure wait_writes(constant expected : in natural) is
            variable waited : natural := 0;
        begin
            while write_count /= expected loop
                wait until falling_edge(rx_clock);
                waited := waited + 1;
                assert waited < MAX_WAIT_CYCLES
                    report "integration FIFO write wait exceeded bound"
                    severity failure;
            end loop;
        end procedure;

        procedure wait_input_ready is
            variable waited : natural := 0;
        begin
            while s_ready /= '1' loop
                wait until falling_edge(rx_clock);
                waited := waited + 1;
                assert waited < MAX_WAIT_CYCLES
                    report "integration ADC-ready wait exceeded bound"
                    severity failure;
            end loop;
        end procedure;

        procedure send_zero_samples(constant count : in positive) is
            variable accepted : natural := 0;
            variable waited : natural := 0;
        begin
            wait until falling_edge(rx_clock);
            s_i <= (others => '0');
            s_q <= (others => '0');
            s_valid <= '1';
            while accepted < count loop
                wait until rising_edge(rx_clock);
                if s_ready = '1' then accepted := accepted + 1; end if;
                waited := waited + 1;
                assert waited < MAX_WAIT_CYCLES
                    report "integration ADC admission exceeded bound"
                    severity failure;
            end loop;
            wait until falling_edge(rx_clock);
            s_valid <= '0';
        end procedure;

        procedure check_config_a is
        begin
            assert fifo_words(0) = x"000220114f425246"
                and fifo_words(1) = x"01312d0000000001"
                and fifo_words(2) = x"89abcdef01234567"
                and fifo_words(3) = x"deadbeef80000001"
                report "first persisted CONFIG differs from literal epoch-1 bytes"
                severity failure;
        end procedure;
    begin
        control_cycles(4);
        control_reset <= '0';
        profile_rx_reset <= '0';
        stream_reset <= '0';
        control_cycles(4);

        commit_bank(WORDS_A, 1);

        -- Model the GPIO_SYS_RST pulse that precedes ordinary first RX enable.
        -- It clears only stream state; the committed bank and armed token must
        -- survive because their image-lifetime resets remain deasserted.
        wait until falling_edge(rx_clock);
        stream_reset <= '1';
        rx_cycles(3);
        stream_reset <= '0';
        rx_cycles(2);

        -- wfull is high while the target FIFO's synchronized write clear is
        -- releasing. Slot 0 may be retained locally, but no FIFO write or ADC
        -- admission is permitted until the real write side reports ready.
        rx_enable <= '1';
        rx_cycles(16);
        assert write_count = 0 and s_ready = '0'
            report "CONFIG escaped FIFO reset/full backpressure" severity failure;
        fifo_full <= '0';
        wait_writes(4);
        check_config_a;
        wait_input_ready;

        send_zero_samples(20);
        wait_writes(8);
        assert fifo_words(4) = x"000220014f425246"
            and fifo_words(5) = x"0000000000000013"
            and fifo_words(6) = x"0000000000000000"
            and fifo_words(7) = x"0000000000000000"
            report "first persisted DATA differs from literal index-19 bytes"
            severity failure;

        -- Fill/backpressure the FIFO as another DATA record starts. Stopping
        -- RX must discard any locally retained half pair. The first write of
        -- the next epoch must be its CONFIG slots 0/1, never stale DATA.
        fifo_full <= '1';
        send_zero_samples(20);
        rx_cycles(24);
        assert write_count = 8
            report "blocked DATA unexpectedly wrote to a full FIFO"
            severity failure;
        rx_enable <= '0';
        rx_cycles(8);
        control_cycles(8);

        commit_bank(WORDS_B, 2);
        rx_enable <= '1';
        rx_cycles(16);
        assert write_count = 8 and s_ready = '0'
            report "restart CONFIG escaped FIFO reset/full backpressure"
            severity failure;
        fifo_full <= '0';
        wait_writes(12);
        assert fifo_words(8) = x"000220124f425246"
            and fifo_words(9) = x"01312d0000000002"
            and fifo_words(10) = x"3333444411112222"
            and fifo_words(11) = x"7777888855556666"
            report "restart did not begin with exact epoch-2 CONFIG"
            severity failure;

        -- A genuine image/POR reset clears the committed bank and start token.
        -- After re-enabling without a new commit, exactly one diagnostic CONFIG
        -- reaches the FIFO and DATA remains closed.
        rx_enable <= '0';
        fifo_full <= '1';
        rx_cycles(8);
        control_cycles(8);
        commit_bank(WORDS_C, 3);
        wait until falling_edge(control_clock);
        control_reset <= '1';
        stream_reset <= '1';
        wait until falling_edge(rx_clock);
        profile_rx_reset <= '1';
        rx_cycles(4);
        control_cycles(4);
        control_reset <= '0';
        profile_rx_reset <= '0';
        stream_reset <= '0';
        rx_cycles(4);

        rx_enable <= '1';
        rx_cycles(16);
        assert write_count = 12 and s_ready = '0'
            report "diagnostic CONFIG escaped FIFO reset/full backpressure"
            severity failure;
        fifo_full <= '0';
        wait_writes(16);
        assert fifo_words(12) = x"000220504f425246"
            and fifo_words(13) = x"01312d0000000000"
            and fifo_words(14) = x"0000000000000000"
            and fifo_words(15) = x"0000000000000000"
            report "cold-reset start did not emit exact diagnostic CONFIG"
            severity failure;
        rx_cycles(40);
        assert write_count = 16 and s_ready = '0'
            report "diagnostic epoch emitted DATA or reopened input"
            severity failure;

        stop;
        wait;
    end process;
end architecture;
