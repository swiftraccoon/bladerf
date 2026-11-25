-- Ready/valid and byte-order contract for the profile-2 two-slot FIFO packer.

library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;
use std.env.all;

entity rf_profile2_slot_packer_tb is end entity;

architecture simulation of rf_profile2_slot_packer_tb is
    signal clock : std_logic := '0';
    signal reset : std_logic := '1';
    signal m_valid : std_logic := '0';
    signal m_ready : std_logic;
    signal m_i, m_q : signed(15 downto 0) := (others => '0');
    signal fifo_full : std_logic := '1';
    signal fifo_write : std_logic;
    signal fifo_data : std_logic_vector(63 downto 0);
    signal writes : natural := 0;
begin
    clock <= not clock after 5 ns;

    dut : entity work.rf_profile2_slot_packer
        port map (
            clock => clock,
            reset => reset,
            m_valid => m_valid,
            m_ready => m_ready,
            m_i => m_i,
            m_q => m_q,
            fifo_full => fifo_full,
            fifo_write => fifo_write,
            fifo_data => fifo_data
        );

    monitor : process(clock)
    begin
        if rising_edge(clock) then
            assert fifo_write = '0' or (
                reset = '0' and fifo_full = '0'
                and m_valid = '1' and m_ready = '1'
            ) report "FIFO write was not the accepted second-slot event"
                severity failure;
            if fifo_write = '1' then
                writes <= writes + 1;
            end if;
        end if;
    end process;

    stimulus : process
        procedure drive_slot(
            constant value_i : in std_logic_vector(15 downto 0);
            constant value_q : in std_logic_vector(15 downto 0)
        ) is
        begin
            m_i <= signed(value_i);
            m_q <= signed(value_q);
            m_valid <= '1';
            wait for 1 ns;
        end procedure;

        procedure release_slot is
        begin
            wait until falling_edge(clock);
            m_valid <= '0';
            wait for 1 ns;
        end procedure;
    begin
        wait until falling_edge(clock);
        assert m_ready = '0' and fifo_write = '0'
            report "reset did not close packer outputs" severity failure;
        reset <= '0';
        wait for 1 ns;

        -- The first slot is elastic even while the real FIFO reports full
        -- during capacity pressure or synchronized clear release.
        drive_slot(x"1122", x"3344");
        assert m_ready = '1' and fifo_write = '0'
            report "empty half buffer did not accept first blocked-FIFO slot"
            severity failure;
        wait until rising_edge(clock);
        wait until falling_edge(clock);

        -- A second slot must remain unaccepted and stable upstream until the
        -- FIFO is write-ready. No speculative write is allowed.
        drive_slot(x"5566", x"7788");
        for cycle in 1 to 3 loop
            assert m_ready = '0' and fifo_write = '0'
                report "second slot escaped FIFO-full backpressure"
                severity failure;
            wait until rising_edge(clock);
            wait until falling_edge(clock);
        end loop;
        fifo_full <= '0';
        wait for 1 ns;
        assert m_ready = '1' and fifo_write = '1'
            report "second-slot acceptance did not assert same-edge FIFO write"
            severity failure;
        assert fifo_data = x"7788556633441122"
            report "first FIFO pair has wrong slot or halfword order"
            severity failure;
        wait until rising_edge(clock);
        release_slot;

        -- Continuous ready input produces one FIFO word for each pair.
        drive_slot(x"99aa", x"bbcc");
        assert m_ready = '1' and fifo_write = '0'
            report "new first slot was not accepted" severity failure;
        wait until rising_edge(clock);
        wait until falling_edge(clock);
        drive_slot(x"ddee", x"f001");
        assert m_ready = '1' and fifo_write = '1'
            report "continuous second slot did not write" severity failure;
        assert fifo_data = x"f001ddeebbcc99aa"
            report "second FIFO pair has wrong slot or halfword order"
            severity failure;
        wait until rising_edge(clock);
        release_slot;

        -- Reset must discard an unpaired slot. It must never be combined with
        -- the first slot accepted in the next epoch.
        drive_slot(x"1234", x"5678");
        wait until rising_edge(clock);
        wait until falling_edge(clock);
        m_valid <= '0';
        reset <= '1';
        wait for 1 ns;
        assert m_ready = '0' and fifo_write = '0'
            report "reset exposed a pending half pair" severity failure;
        wait until rising_edge(clock);
        wait until falling_edge(clock);
        reset <= '0';
        wait for 1 ns;

        drive_slot(x"0a0b", x"0c0d");
        wait until rising_edge(clock);
        wait until falling_edge(clock);
        drive_slot(x"0e0f", x"1011");
        assert fifo_write = '1' and fifo_data = x"10110e0f0c0d0a0b"
            report "reset did not discard the old unpaired slot"
            severity failure;
        wait until rising_edge(clock);
        release_slot;

        wait until falling_edge(clock);
        assert writes = 3
            report "packer emitted a missing or duplicate FIFO write"
            severity failure;
        stop;
        wait;
    end process;
end architecture;
