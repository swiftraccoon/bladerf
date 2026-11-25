-- Lossless two-slot adapter from the profile-2 ready/valid stream to the
-- native bladeRF RX FIFO's 64-bit write port. A complex int16 slot occupies
-- one 32-bit host word with I in the low half and Q in the high half. The
-- lower FIFO word is therefore the first accepted slot.
--
-- fifo_full must be the actual write-domain full output of the target FIFO.
-- The pinned Cyclone-V DCFIFO also holds that output high while its synchronized
-- write-side asynchronous clear is releasing. The empty half buffer may retain
-- one slot during that interval, but a pair is accepted and written only when
-- fifo_full is low. Reset discards an unpaired slot and suppresses writes.

library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;

entity rf_profile2_slot_packer is
    port (
        clock : in std_logic;
        reset : in std_logic;

        m_valid : in std_logic;
        m_ready : out std_logic;
        m_i : in signed(15 downto 0);
        m_q : in signed(15 downto 0);

        fifo_full : in std_logic;
        fifo_write : out std_logic;
        fifo_data : out std_logic_vector(63 downto 0)
    );
end entity;

architecture rtl of rf_profile2_slot_packer is
    signal half_valid : std_logic := '0';
    signal first_i : signed(15 downto 0) := (others => '0');
    signal first_q : signed(15 downto 0) := (others => '0');
    signal ready_i : std_logic;
begin
    ready_i <= '0' when reset = '1' else
        '1' when half_valid = '0' else
        '1' when fifo_full = '0' else '0';
    m_ready <= ready_i;

    -- The second-slot handshake and FIFO write are the same event. In
    -- particular, accepting CONFIG slot 7 writes the final CONFIG pair.
    fifo_write <= '1' when reset = '0' and half_valid = '1'
        and m_valid = '1' and fifo_full = '0' else '0';
    fifo_data <= std_logic_vector(m_q) & std_logic_vector(m_i)
        & std_logic_vector(first_q) & std_logic_vector(first_i);

    retain_first_slot : process(clock)
    begin
        if rising_edge(clock) then
            if reset = '1' then
                half_valid <= '0';
                first_i <= (others => '0');
                first_q <= (others => '0');
            elsif m_valid = '1' and ready_i = '1' then
                if half_valid = '0' then
                    first_i <= m_i;
                    first_q <= m_q;
                    half_valid <= '1';
                else
                    half_valid <= '0';
                end if;
            end if;
        end if;
    end process;
end architecture;
